//! A resource backend served wholly by the engine at one home (#3108): the decided store's own
//! kinds and lifecycle views ([`EngineBackend`]), with its Convoy and Message paths over the same
//! store, as a daemon composes them. Records reach it the ways they reach a daemon: written
//! through the backend, admitted as launches ([`EngineHome::admit`]), or replicated from another
//! home's log ([`EngineHome::replicate_to`], [`EngineBackend::receive`]).

use std::sync::Arc;

use flotilla_engine_messages_testkit::EngineMessagesHome;
use flotilla_provisioning_testkit::{EngineBackend, ENGINE_NAMESPACE};
use flotilla_resources::{Clock, Convoy, ConvoySpec, ConvoyStatus, HostSpec, HostStatus, ResourceObject, SystemClock};
use flotilla_schema_sqlite::stream::Service;
use flotilla_store::ResourceBackend;

use crate::EngineConvoysHome;

/// One home's engine on a resource backend; the decided store lives as long as this value.
pub struct EngineHome {
    pub engine: EngineBackend,
    pub convoys: EngineConvoysHome,
    pub messages: EngineMessagesHome,
    namespace: String,
}

impl EngineHome {
    /// The engine at `home`, serving every kind in [`ENGINE_NAMESPACE`].
    pub fn open(home: &str) -> Self {
        Self::open_in(home, ENGINE_NAMESPACE)
    }

    /// The engine at `home`, serving every kind in `namespace`.
    pub fn open_in(home: &str, namespace: &str) -> Self {
        Self::open_with(home, namespace, Arc::new(SystemClock))
    }

    /// The engine at `home`, serving every kind in `namespace`, its records stamped by `clock`.
    pub fn open_with(home: &str, namespace: &str, clock: Arc<dyn Clock>) -> Self {
        let engine = EngineBackend::open_with(home, namespace, Arc::clone(&clock));
        let convoys = EngineConvoysHome::over(&engine.service, namespace, &engine.backend, Arc::clone(&clock), engine.reads());
        let messages = EngineMessagesHome::over_with_clock(Arc::clone(&engine.service), namespace, &engine.backend, clock);
        Self { engine, convoys, messages, namespace: namespace.to_owned() }
    }

    /// The backend every kind is served on.
    pub fn backend(&self) -> &ResourceBackend {
        &self.engine.backend
    }

    /// The decided store: another home's log is replicated into it as a peer's arrives.
    pub fn service(&self) -> &Arc<Service> {
        &self.engine.service
    }

    /// Admits convoy `name` with `spec` and `status` as the next launch of `name`, the way
    /// admission does; returns the launch.
    pub async fn admit(&self, name: &str, spec: ConvoySpec, status: ConvoyStatus) -> String {
        self.convoys.launch(crate::convoy(&self.namespace, name, spec, status)).await
    }

    /// Replicates this home's log into `target`, as a peer's arrives: what this home wrote reads
    /// there as another home's.
    pub fn replicate_to(&self, target: &EngineHome) {
        flotilla_provisioning_testkit::replicate_home(self.service(), self.engine.home(), target.service());
    }

    /// Host `name` written at its own home, as its daemon publishes it, and received here.
    pub async fn put_host_at_its_home(&self, name: &str, spec: &HostSpec, status: Option<&HostStatus>) {
        self.engine.put_host_at_its_home(name, spec, status).await;
    }

    /// The launch `launch` as the backend reads it.
    pub async fn convoy(&self, launch: &str) -> ResourceObject<Convoy> {
        self.backend().using::<Convoy>(&self.namespace).get(launch).await.expect("read the launch")
    }
}

/// A test reads and writes through the backend; the value keeps its store open.
impl std::ops::Deref for EngineHome {
    type Target = ResourceBackend;

    fn deref(&self) -> &ResourceBackend {
        &self.engine.backend
    }
}
