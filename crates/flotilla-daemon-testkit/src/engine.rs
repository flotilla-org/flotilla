//! The engine a test daemon reads (#3108): a daemon the runtime does not compose still reads
//! and writes its environments, clones and checkouts on its decided store.

use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};

use flotilla_core::in_process::InProcessDaemon;
use flotilla_provisioning_testkit::{EngineFixture, EnvironmentFixture};
use flotilla_schema_sqlite::stream::Service;

/// An engine installed into a test daemon, by the decided store it is on.
type Installed = (Weak<Service>, Arc<EngineFixture>);

/// The engines installed into test daemons.
static ENGINES: LazyLock<Mutex<Vec<Installed>>> = LazyLock::new(Default::default);

/// The engine `daemon` reads, on its decided store: its environments, clones and checkouts,
/// stepped as their controllers would step them. Installed as its lifecycle reads on first use,
/// and the same engine after, as a composed daemon has one.
pub fn engine(daemon: &InProcessDaemon) -> Arc<EngineFixture> {
    let service = daemon.decided_store().expect("decided store");
    let mut engines = ENGINES.lock().unwrap_or_else(PoisonError::into_inner);
    engines.retain(|(store, _)| store.strong_count() > 0);
    if let Some((_, engine)) = engines.iter().find(|(store, _)| store.as_ptr() == Arc::as_ptr(&service)) {
        return Arc::clone(engine);
    }
    let engine = Arc::new(EngineFixture::over(service.home(), service.store()));
    daemon.install_lifecycle_reads(engine.reads()).expect("install the engine's readings");
    engines.push((Arc::downgrade(&service), Arc::clone(&engine)));
    engine
}

/// Checkouts written at the home of a daemon the runtime composed, on its decided store, as its
/// Checkout controller writes them.
pub fn runtime_checkouts(daemon: &InProcessDaemon) -> flotilla_provisioning_testkit::CheckoutFixture {
    let service = daemon.decided_store().expect("decided store");
    flotilla_provisioning_testkit::CheckoutFixture::over(service.home(), service.store())
}

/// The environments of `daemon`'s [`engine`].
pub fn engine_environments(daemon: &InProcessDaemon) -> Environments {
    Environments(engine(daemon))
}

/// An engine's environments, the engine held with them.
pub struct Environments(Arc<EngineFixture>);

impl std::ops::Deref for Environments {
    type Target = EnvironmentFixture;

    fn deref(&self) -> &EnvironmentFixture {
        &self.0.environments
    }
}
