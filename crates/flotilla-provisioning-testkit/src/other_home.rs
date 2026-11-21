//! Another daemon's decided store, for tests of one daemon that reads records homed elsewhere.
//!
//! On the engine a record is written only at its home (ADR 0064): a Host by its own daemon, an
//! SSH host by the daemon that owns it. A test that needs another host's records writes them at
//! an [`OtherHome`], as that daemon would, and replicates them into the daemon under test's store
//! the way a peer's log arrives: a snapshot round of that home, received at the target.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock, PoisonError, Weak},
};

use chrono::{DateTime, Utc};
use flotilla_provisioning::engine::{
    definitions::EngineDefinitions, hosts::EngineHosts, projects::EngineProjects, repositories::EngineRepositories,
};
use flotilla_resources::{
    engine_records::KindRecords, HostConnection, HostSpec, HostStatus, ProjectSpec, RepositorySpec, Resource, SystemClock,
};
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::stream::{Follower, Service};

type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;

/// A decided store homed at `home`, in its own temporary directory or beside the store it writes for.
pub struct OtherHome {
    home: String,
    service: Arc<Service>,
    _dir: Option<tempfile::TempDir>,
}

impl OtherHome {
    pub fn open(home: &str) -> Self {
        let dir = tempfile::TempDir::new().expect("other home directory");
        let mut opened = Self::open_at(&dir.path().join("decided.sqlite"), home);
        opened._dir = Some(dir);
        opened
    }

    /// Opens the home in `target`'s directory, so it goes when the test removes that directory.
    fn open_beside(target: &Service, home: &str) -> Self {
        let target_path = target
            .with_store(|store| Ok(store.connection().path().map(std::path::PathBuf::from)))
            .expect("the target store's path")
            .filter(|path| !path.as_os_str().is_empty())
            .expect("a target store in a file");
        let directory = target_path.parent().expect("the target store's directory");
        Self::open_at(&directory.join(format!("other-home-{home}.sqlite")), home)
    }

    fn open_at(path: &std::path::Path, home: &str) -> Self {
        let registry = flotilla_provisioning::stored::decided_registry().expect("decided registry");
        let service = Arc::new(Service::open(path, home, registry).expect("other home store"));
        Self { home: home.to_owned(), service, _dir: None }
    }

    pub fn home(&self) -> &str {
        &self.home
    }

    pub fn service(&self) -> &Arc<Service> {
        &self.service
    }

    /// Writes Host `name` here as the daemon it is homed at would: its own daemon, or the daemon
    /// owning an SSH host. The status, when given, is that daemon's observation of it.
    pub async fn host(&self, name: &str, spec: &HostSpec, status: Option<&HostStatus>) {
        let own_host = match &spec.connection {
            HostConnection::Daemon => name.to_owned(),
            HostConnection::SshHost { owning_daemon, .. } => owning_daemon.clone(),
        };
        let hosts = EngineHosts::open(Arc::clone(&self.service), own_host, Arc::new(SystemClock)).expect("other home hosts");
        let written = hosts.apply(name, None, &Default::default(), &Default::default(), spec).await.expect("write host at its home");
        if let Some(status) = status {
            hosts.set_status(name, written.version, status).await.expect("observe host at its home");
        }
    }

    /// Writes FulfilmentKind `name` here, as its home's operator would declare it.
    pub async fn kind(&self, name: &str, spec: &flotilla_resources::FulfilmentKindSpec) {
        let kinds = flotilla_provisioning::engine::kinds::EngineKinds::open(Arc::clone(&self.service), Arc::new(SystemClock))
            .expect("other home kinds");
        kinds.apply(name, None, &Default::default(), &Default::default(), spec).await.expect("write kind at its home");
    }

    /// Writes Repository `name` here, as this home's observer or operator would.
    pub async fn repository(&self, name: &str, spec: &RepositorySpec) {
        let repositories = EngineRepositories::open(Arc::clone(&self.service), Arc::new(SystemClock)).expect("other home repositories");
        repositories.apply(name, None, &Default::default(), &Default::default(), spec).await.expect("write repository at its home");
    }

    /// Records what this home's Repository reconciler observed of Repository `name`.
    pub async fn repository_status(&self, name: &str, status: &flotilla_resources::RepositoryStatus) {
        let repositories = EngineRepositories::open(Arc::clone(&self.service), Arc::new(SystemClock)).expect("other home repositories");
        let held = repositories.get(name).await.expect("read repository at its home").expect("held at its home");
        repositories.set_status(name, held.version, status).await.expect("observe repository at its home");
    }

    /// Writes Project `name` here with its writer's stamps, as this home declares it.
    pub async fn project(&self, name: &str, labels: &BTreeMap<String, String>, annotations: &BTreeMap<String, String>, spec: &ProjectSpec) {
        let projects = EngineProjects::open(Arc::clone(&self.service), Arc::new(SystemClock)).expect("other home projects");
        projects.apply(name, None, labels, annotations, spec).await.expect("write project at its home");
    }

    /// Writes definition `name` of kind `R` (a Forge, a credential declaration, ...) here, as this
    /// home's operator would declare it.
    pub async fn definition<R: Resource>(&self, name: &str, spec: &R::Spec)
    where
        EngineDefinitions: KindRecords<R>,
    {
        self.stamped_definition::<R>(name, &BTreeMap::new(), spec).await;
    }

    /// [`OtherHome::definition`], with the stamps its writer puts on it.
    pub async fn stamped_definition<R: Resource>(&self, name: &str, annotations: &BTreeMap<String, String>, spec: &R::Spec)
    where
        EngineDefinitions: KindRecords<R>,
    {
        let definitions = EngineDefinitions::open(Arc::clone(&self.service), Arc::new(SystemClock)).expect("other home definitions");
        KindRecords::<R>::apply(&definitions, name, None, &Default::default(), annotations, spec)
            .await
            .expect("write definition at its home");
    }

    /// Deletes definition `name` of kind `R` here, as this home's operator would.
    pub async fn delete_definition<R: Resource>(&self, name: &str)
    where
        EngineDefinitions: KindRecords<R>,
    {
        let definitions = EngineDefinitions::open(Arc::clone(&self.service), Arc::new(SystemClock)).expect("other home definitions");
        KindRecords::<R>::delete(&definitions, name).await.expect("delete definition at its home");
    }

    /// Sends everything this home holds to `target`, heard now.
    pub fn replicate_to(&self, target: &Service) {
        self.replicate_to_at(target, flotilla_resources::clock::now());
    }

    /// Sends everything this home holds to `target`, heard at `at`.
    pub fn replicate_to_at(&self, target: &Service, at: DateTime<Utc>) {
        replicate_home_at(&self.service, &self.home, target, at);
    }
}

/// Sends everything `from`, homed at `home`, holds to `target`, heard now: one peer's log arriving
/// at another, as a snapshot round. For tests of two daemons' decided stores.
pub fn replicate_home(from: &Service, home: &str, target: &Service) {
    replicate_home_at(from, home, target, flotilla_resources::clock::now());
}

/// [`replicate_home`], heard at `at`.
pub fn replicate_home_at(from: &Service, home: &str, target: &Service, at: DateTime<Utc>) {
    let mut frames = Vec::new();
    from.serve(None, Timestamp::from_datetime(&at), |frame| {
        frames.push(frame.to_vec());
        Ok(())
    })
    .expect("serve home");
    target
        .with_store(|store| Follower::new(store).receive(home, |sink| frames.iter().try_for_each(|frame| sink(frame))))
        .expect("receive home");
}

/// Sends `target` the entries `from`, homed at `home`, has written since `target` last heard it,
/// heard now: a peer's next entries arriving, as a running daemon's follower receives them. A
/// snapshot round, as [`replicate_home`] sends, makes every follower at `target` read again.
pub fn replicate_home_tail(from: &Service, home: &str, target: &Service) {
    let after = target.last_sequence(home).expect("the target's sequence for the home");
    let mut frames = Vec::new();
    from.serve(Some(after), Timestamp::from_datetime(&flotilla_resources::clock::now()), |frame| {
        frames.push(frame.to_vec());
        Ok(())
    })
    .expect("serve home");
    target
        .with_store(|store| Follower::new(store).receive(home, |sink| frames.iter().try_for_each(|frame| sink(frame))))
        .expect("receive home");
}

/// The other homes each target has been sent, by the target and the home: a home keeps its log, so
/// a later write reaches the target as a peer's next entries would.
type Homes = Mutex<Vec<(Weak<Service>, String, Arc<OtherHome>)>>;

fn homes() -> &'static Homes {
    static HOMES: OnceLock<Homes> = OnceLock::new();
    HOMES.get_or_init(Homes::default)
}

/// The home `home` that writes for `target`, opened on first use.
pub fn other_home_for(target: &Arc<Service>, home: &str) -> Arc<OtherHome> {
    let mut homes = homes().lock().unwrap_or_else(PoisonError::into_inner);
    homes.retain(|(target, _, _)| target.strong_count() > 0);
    if let Some((_, _, found)) = homes.iter().find(|(held, name, _)| held.as_ptr() == Arc::as_ptr(target) && name == home) {
        return Arc::clone(found);
    }
    // The registry outlives the test, so the home's files live with the target's, not in a directory of its own.
    let opened = Arc::new(OtherHome::open_beside(target, home));
    homes.push((Arc::downgrade(target), home.to_owned(), Arc::clone(&opened)));
    opened
}

/// Writes Host `name` at its own home (`node-of-<name>`), as its daemon would publish it, and
/// replicates that home into `target`, heard now.
pub async fn put_host_at_its_home(target: &Arc<Service>, name: &str, spec: &HostSpec, status: Option<&HostStatus>) {
    put_host_at_its_home_heard(target, name, spec, status, flotilla_resources::clock::now()).await;
}

/// [`put_host_at_its_home`], its home last heard at `at`.
pub async fn put_host_at_its_home_heard(
    target: &Arc<Service>,
    name: &str,
    spec: &HostSpec,
    status: Option<&HostStatus>,
    at: DateTime<Utc>,
) {
    let home = other_home_for(target, &format!("node-of-{name}"));
    home.host(name, spec, status).await;
    home.replicate_to_at(target, at);
}

/// The ports a [`Seed`] writes through.
pub struct SeedPorts {
    definitions: EngineDefinitions,
    repositories: EngineRepositories,
}

/// One declaration [`seed_home`] writes: a definition kind's, through the definitions port, or a
/// Repository, through the repository port.
pub struct Seed(Box<SeedWrite>);

/// A [`Seed`]'s write through the ports.
type SeedWrite = dyn FnOnce(Arc<SeedPorts>) -> BoxFuture<Result<(), String>> + Send;

impl Seed {
    /// Definition `name` of kind `R`.
    pub fn definition<R: Resource>(name: impl Into<String>, spec: R::Spec) -> Self
    where
        EngineDefinitions: KindRecords<R>,
    {
        let name = name.into();
        Self(Box::new(move |ports| {
            Box::pin(async move {
                KindRecords::<R>::apply(&ports.definitions, &name, None, &Default::default(), &Default::default(), &spec)
                    .await
                    .map(drop)
                    .map_err(String::from)
            })
        }))
    }

    /// Repository `name`.
    pub fn repository(name: impl Into<String>, spec: RepositorySpec) -> Self {
        let name = name.into();
        Self(Box::new(move |ports| {
            Box::pin(async move {
                ports.repositories.apply(&name, None, &Default::default(), &Default::default(), &spec).await.map(drop).map_err(String::from)
            })
        }))
    }
}

/// Writes `seed` into the decided store at `path` homed at `home`, as that home's own
/// declarations, before its daemon opens it: the records a daemon reads while it composes.
pub async fn seed_home(path: &std::path::Path, home: &str, seed: Vec<Seed>) -> Result<(), String> {
    let registry = flotilla_provisioning::stored::decided_registry().map_err(|error| format!("{error:?}"))?;
    let service = Arc::new(Service::open(path, home, registry).map_err(|error| error.to_string())?);
    let ports = Arc::new(SeedPorts {
        definitions: EngineDefinitions::open(Arc::clone(&service), Arc::new(SystemClock))?,
        repositories: EngineRepositories::open(Arc::clone(&service), Arc::new(SystemClock))?,
    });
    for Seed(write) in seed {
        write(Arc::clone(&ports)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Behaviour: a Host written at its own home reads at the home it is replicated to.
    #[tokio::test]
    async fn a_host_written_at_its_home_reads_where_it_is_replicated() {
        let target = OtherHome::open("target");
        let hosts = EngineHosts::open(Arc::clone(target.service()), "target-host".into(), Arc::new(SystemClock)).expect("hosts");
        put_host_at_its_home(target.service(), "feta", &HostSpec::default(), Some(&HostStatus::default())).await;
        let feta = hosts.get("feta").await.expect("read").expect("replicated host");
        assert_eq!(feta.home, "node-of-feta");
        assert!(feta.status.is_some());
        let mut status = HostStatus::default();
        status.capabilities.insert(flotilla_resources::HELD_CREDENTIALS_CAPABILITY.into(), serde_json::json!(["claude-max"]));
        put_host_at_its_home(target.service(), "feta", &HostSpec::default(), Some(&status)).await;
        let feta = hosts.get("feta").await.expect("read").expect("replicated host");
        assert_eq!(
            feta.status.expect("status").capabilities[flotilla_resources::HELD_CREDENTIALS_CAPABILITY],
            serde_json::json!(["claude-max"])
        );
    }
}
