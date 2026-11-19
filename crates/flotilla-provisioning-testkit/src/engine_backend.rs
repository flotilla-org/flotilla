//! A resource backend served by the engine, as a daemon composes one (#3108): a decided store at
//! one home, its own kinds' ports and the lifecycle views over its launches, installed the way the
//! composition root installs them. Convoy and Message are served by their own paths, which a test
//! installs over [`EngineBackend::service`] (the engine-convoys and engine-messages testkits).

use std::sync::Arc;

use flotilla_observation::forge_observation::DaemonSourceObservers;
use flotilla_provisioning::engine::{
    decided::{open_ports, DecidedPorts},
    repositories::EngineRepositories,
};
use flotilla_resources::{lifecycle_reads::LifecycleReads, Clock, HostSpec, HostStatus, SystemClock};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::{LifecycleViews, ResourceBackend, TerminalViews, VesselViews};

use crate::{other_home_for, put_host_at_its_home, EngineFixture, OtherHome};

/// The namespace the engine serves its kinds in, as a daemon's provisioning namespace.
pub const ENGINE_NAMESPACE: &str = "flotilla";

/// One home's engine on a resource backend; its decided store lives as long as this value.
pub struct EngineBackend {
    pub backend: ResourceBackend,
    pub service: Arc<Service>,
    pub ports: DecidedPorts,
    /// The lifecycle records the views read, which a test drives.
    pub engine: EngineFixture,
    _dir: tempfile::TempDir,
}

impl EngineBackend {
    /// The engine at `home`, serving its kinds in [`ENGINE_NAMESPACE`].
    pub fn open(home: &str) -> Self {
        Self::open_in(home, ENGINE_NAMESPACE)
    }

    /// The engine at `home`, serving its kinds in `namespace`.
    pub fn open_in(home: &str, namespace: &str) -> Self {
        Self::open_with(home, namespace, Arc::new(SystemClock))
    }

    /// The engine at `home`, serving its kinds in `namespace`, its records stamped by `clock`.
    pub fn open_with(home: &str, namespace: &str, clock: Arc<dyn Clock>) -> Self {
        let dir = tempfile::tempdir().expect("decided store directory");
        let registry = flotilla_provisioning::stored::decided_registry().expect("decided store kinds");
        let service = Arc::new(Service::open(&dir.path().join("decided.sqlite"), home, registry).expect("decided store"));
        let backend = flotilla_store_memory::engine_home_backend().with_local_root(flotilla_protocol::NodeId::new(home));
        backend.serve_engine_in(namespace);
        let ports = open_ports(&service, home, clock).expect("decided store ports");
        for records in &ports.records {
            backend.install_engine_records(Arc::clone(records));
        }
        backend.install_engine_records(Arc::clone(&ports.observations) as _);
        // Forge sources are this home's as the daemon judges them: every source no other home's
        // observer holds.
        let judged = DaemonSourceObservers::new(&backend, Arc::new(std::sync::RwLock::new(namespace.to_owned())));
        ports.source_observers.install(Arc::new(judged)).expect("source observers installed once");
        let engine = EngineFixture::over_service(&service);
        let reads = engine.reads();
        backend.install_engine_records(Arc::new(LifecycleViews::new(Arc::clone(&reads))));
        backend.install_engine_records(Arc::new(TerminalViews::new(namespace, Arc::clone(&reads))));
        backend.install_engine_records(Arc::new(VesselViews::new(backend.clone(), namespace, reads)));
        Self { backend, service, ports, engine, _dir: dir }
    }

    /// The lifecycle readings the views serve, which the Convoy path composes with.
    pub fn reads(&self) -> Arc<dyn LifecycleReads> {
        self.engine.reads()
    }

    /// This home: the node its records are written at.
    pub fn home(&self) -> &str {
        self.service.home()
    }

    /// Another home, whose records reach this one as its log arrives ([`EngineBackend::receive`]).
    pub fn other_home(&self, home: &str) -> Arc<OtherHome> {
        other_home_for(&self.service, home)
    }

    /// Receives `other`'s log, as a peer's arrives: what it wrote reads here as its records.
    pub fn receive(&self, other: &OtherHome) {
        other.replicate_to(&self.service);
    }

    /// Host `name` written at its own home, as its daemon publishes it, and received here.
    pub async fn put_host_at_its_home(&self, name: &str, spec: &HostSpec, status: Option<&HostStatus>) {
        put_host_at_its_home(&self.service, name, spec, status).await;
    }

    /// A second handle on this home's Repository port, for a test that wraps the port it installs.
    pub fn repository_port(&self) -> EngineRepositories {
        EngineRepositories::open(Arc::clone(&self.service), Arc::new(SystemClock)).expect("the engine's Repository port")
    }
}

/// A test reads and writes through the backend; the value keeps its store open.
impl std::ops::Deref for EngineBackend {
    type Target = ResourceBackend;

    fn deref(&self) -> &ResourceBackend {
        &self.backend
    }
}
