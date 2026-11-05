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
use flotilla_provisioning::engine::{hosts::EngineHosts, projects::EngineProjects, repositories::EngineRepositories};
use flotilla_resources::{engine_records::EngineRecords, HostConnection, HostSpec, HostStatus, ProjectSpec, RepositorySpec, SystemClock};
use flotilla_schema::Timestamp;
use flotilla_schema_sqlite::stream::{Follower, Service};

/// A decided store homed at `home`, in a temporary directory.
pub struct OtherHome {
    home: String,
    service: Arc<Service>,
    _dir: tempfile::TempDir,
}

impl OtherHome {
    pub fn open(home: &str) -> Self {
        let dir = tempfile::TempDir::new().expect("other home directory");
        let registry = flotilla_provisioning::stored::decided_registry().expect("decided registry");
        let service = Arc::new(Service::open(&dir.path().join("decided.sqlite"), home, registry).expect("other home store"));
        Self { home: home.to_owned(), service, _dir: dir }
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
        let spec_value = serde_json::to_value(spec).expect("host spec");
        hosts.apply("Host", name, &Default::default(), &Default::default(), spec_value).await.expect("write host at its home");
        if let Some(status) = status {
            hosts.set_status("Host", name, serde_json::to_value(status).expect("host status")).await.expect("observe host at its home");
        }
    }

    /// Writes FulfilmentKind `name` here, as its home's operator would declare it.
    pub async fn kind(&self, name: &str, spec: &flotilla_resources::FulfilmentKindSpec) {
        let kinds = flotilla_provisioning::engine::kinds::EngineKinds::open(Arc::clone(&self.service), Arc::new(SystemClock))
            .expect("other home kinds");
        let spec = serde_json::to_value(spec).expect("kind spec");
        kinds.apply("FulfilmentKind", name, &Default::default(), &Default::default(), spec).await.expect("write kind at its home");
    }

    /// Writes Repository `name` here, as this home's observer or operator would.
    pub async fn repository(&self, name: &str, spec: &RepositorySpec) {
        let repositories = EngineRepositories::open(Arc::clone(&self.service), Arc::new(SystemClock)).expect("other home repositories");
        let spec = serde_json::to_value(spec).expect("repository spec");
        repositories.apply("Repository", name, &Default::default(), &Default::default(), spec).await.expect("write repository at its home");
    }

    /// Writes Project `name` here with its writer's stamps, as this home declares it.
    pub async fn project(&self, name: &str, labels: &BTreeMap<String, String>, annotations: &BTreeMap<String, String>, spec: &ProjectSpec) {
        let projects = EngineProjects::open(Arc::clone(&self.service), Arc::new(SystemClock)).expect("other home projects");
        let spec = serde_json::to_value(spec).expect("project spec");
        projects.apply("Project", name, labels, annotations, spec).await.expect("write project at its home");
    }

    /// Writes definition `name` of `kind` (a Forge, a credential declaration, ...) here, as this
    /// home's operator would declare it.
    pub async fn definition(&self, kind: &str, name: &str, spec: serde_json::Value) {
        let definitions =
            flotilla_provisioning::engine::definitions::EngineDefinitions::open(Arc::clone(&self.service), Arc::new(SystemClock))
                .expect("other home definitions");
        definitions.apply(kind, name, &Default::default(), &Default::default(), spec).await.expect("write definition at its home");
    }

    /// Sends everything this home holds to `target`, heard now.
    pub fn replicate_to(&self, target: &Service) {
        self.replicate_to_at(target, Utc::now());
    }

    /// Sends everything this home holds to `target`, heard at `at`.
    pub fn replicate_to_at(&self, target: &Service, at: DateTime<Utc>) {
        replicate_home_at(&self.service, &self.home, target, at);
    }
}

/// Sends everything `from`, homed at `home`, holds to `target`, heard now: one peer's log arriving
/// at another, as a snapshot round. For tests of two daemons' decided stores.
pub fn replicate_home(from: &Service, home: &str, target: &Service) {
    replicate_home_at(from, home, target, Utc::now());
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
    let opened = Arc::new(OtherHome::open(home));
    homes.push((Arc::downgrade(target), home.to_owned(), Arc::clone(&opened)));
    opened
}

/// Writes Host `name` at its own home (`node-of-<name>`), as its daemon would publish it, and
/// replicates that home into `target`, heard now.
pub async fn put_host_at_its_home(target: &Arc<Service>, name: &str, spec: &HostSpec, status: Option<&HostStatus>) {
    put_host_at_its_home_heard(target, name, spec, status, Utc::now()).await;
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

/// Writes `seed`, each a kind, name and spec, into the decided store at `path` homed at `home`, as
/// that home's own declarations, before its daemon opens it: the records a daemon reads while it
/// composes. Repositories go through the repository port, every other kind through the
/// definitions port.
pub async fn seed_home(path: &std::path::Path, home: &str, seed: Vec<(&'static str, String, serde_json::Value)>) -> Result<(), String> {
    use flotilla_provisioning::engine::{definitions::EngineDefinitions, repositories::EngineRepositories};

    let registry = flotilla_provisioning::stored::decided_registry().map_err(|error| format!("{error:?}"))?;
    let service = Arc::new(Service::open(path, home, registry).map_err(|error| error.to_string())?);
    let definitions = EngineDefinitions::open(Arc::clone(&service), Arc::new(SystemClock))?;
    let repositories = EngineRepositories::open(Arc::clone(&service), Arc::new(SystemClock))?;
    for (kind, name, spec) in seed {
        let port: &dyn EngineRecords = if kind == "Repository" { &repositories } else { &definitions };
        port.apply(kind, &name, &Default::default(), &Default::default(), spec).await?;
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
        let feta = hosts.get("Host", "feta").await.expect("read").expect("replicated host");
        assert_eq!(feta.home, "node-of-feta");
        assert!(feta.status.is_some());
        let mut status = HostStatus::default();
        status.capabilities.insert(flotilla_resources::HELD_CREDENTIALS_CAPABILITY.into(), serde_json::json!(["claude-max"]));
        put_host_at_its_home(target.service(), "feta", &HostSpec::default(), Some(&status)).await;
        let feta = hosts.get("Host", "feta").await.expect("read").expect("replicated host");
        assert_eq!(
            feta.status.expect("status")["capabilities"][flotilla_resources::HELD_CREDENTIALS_CAPABILITY],
            serde_json::json!(["claude-max"])
        );
    }
}
