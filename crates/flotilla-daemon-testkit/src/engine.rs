//! The engine a test daemon reads (#3108): a daemon the runtime does not compose still reads
//! and writes its environments, clones and checkouts on its decided store.

use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};

use flotilla_core::in_process::InProcessDaemon;
use flotilla_provisioning_testkit::{EngineFixture, EnvironmentFixture};
use flotilla_resources::{
    terminal_records::{TerminalLaunchFacts, TerminalLaunchRequest, TerminalLaunchSource},
    TerminalSessionSource, TerminalSessionSpec,
};
use flotilla_schema_sqlite::stream::Service;

/// An engine installed into a test daemon, by the decided store it is on.
type Installed = (Weak<Service>, Arc<EngineFixture>);

/// The engines installed into test daemons.
static ENGINES: LazyLock<Mutex<Vec<Installed>>> = LazyLock::new(Default::default);

/// The engine `daemon` reads, on its decided store: its environments, clones, checkouts and
/// terminal sessions, stepped as their controllers would step them, and the terminal sessions of
/// every other home replicated into it, followed so their watchers hear each change. Installed as
/// its lifecycle reads on first use,
/// and the same engine after, as a composed daemon has one.
pub fn engine(daemon: &InProcessDaemon) -> Arc<EngineFixture> {
    let service = daemon.decided_store().expect("decided store");
    let mut engines = ENGINES.lock().unwrap_or_else(PoisonError::into_inner);
    engines.retain(|(store, _)| store.strong_count() > 0);
    if let Some((_, engine)) = engines.iter().find(|(store, _)| store.as_ptr() == Arc::as_ptr(&service)) {
        return Arc::clone(engine);
    }
    let engine = Arc::new(EngineFixture::over_service(&service));
    // Its terminal sessions', vessels', environments' and checkouts' readers and watchers hear each
    // launch change, as the runtime's do. The followers run on the test's runtime and end with it.
    if tokio::runtime::Handle::try_current().is_ok() {
        drop(engine.terminals.terminals.follow(Arc::clone(&service)).expect("follow the terminal sessions"));
        drop(engine.vessels.vessels.follow(Arc::clone(&service)).expect("follow the vessels"));
        drop(engine.environments.environments.follow(Arc::clone(&service)).expect("follow the environments"));
        drop(engine.checkouts.checkouts.follow(Arc::clone(&service)).expect("follow the checkouts"));
    }
    daemon.install_lifecycle_reads(engine.reads()).expect("install the engine's readings");
    engines.push((Arc::downgrade(&service), Arc::clone(&engine)));
    engine
}

/// Requests session `name` at `daemon`'s [`engine`] as old TerminalSession `spec` describes it,
/// returning its launch.
pub fn request_session(daemon: &InProcessDaemon, name: &str, spec: &TerminalSessionSpec) -> String {
    engine(daemon).terminals.request(&session_request(name, spec))
}

/// The launch request for session `name` that old TerminalSession `spec` describes: a crew's
/// session is held for its crew context's vessel, a tool for no one.
pub fn session_request(name: &str, spec: &TerminalSessionSpec) -> TerminalLaunchRequest {
    let (source, vessel) = match &spec.source {
        TerminalSessionSource::Tool { command } => (TerminalLaunchSource::Tool { command: command.clone() }, None),
        TerminalSessionSource::Agent { selector, brief, context, .. } => (
            TerminalLaunchSource::Agent {
                capability: selector.capability.clone(),
                adapter: selector.adapter.clone(),
                model: selector.model.clone(),
                brief: brief.clone(),
            },
            Some(context.vessel_ref.clone()),
        ),
    };
    TerminalLaunchRequest {
        session: name.to_owned(),
        environment: spec.env_ref.clone(),
        vessel,
        role: spec.role.clone(),
        pool: spec.pool.clone(),
        cwd: spec.cwd.clone(),
        env: spec.env.clone(),
        source,
    }
}

/// Session `name` running at `daemon`'s [`engine`] as old TerminalSession `spec` describes it:
/// requested, and realised as pool session `name` with `facts`. Returns its launch.
pub fn run_session(daemon: &InProcessDaemon, name: &str, spec: &TerminalSessionSpec, facts: &TerminalLaunchFacts) -> String {
    let launch = request_session(daemon, name, spec);
    engine(daemon).terminals.ready(&launch, name, facts);
    launch
}

/// Checkouts written at the home of a daemon the runtime composed, on its decided store, as its
/// Checkout controller writes them.
pub fn runtime_checkouts(daemon: &InProcessDaemon) -> flotilla_provisioning_testkit::CheckoutFixture {
    let service = daemon.decided_store().expect("decided store");
    flotilla_provisioning_testkit::CheckoutFixture::over(service.home(), service.store())
}

/// Terminal sessions written at the home of a daemon the runtime composes, on its decided store,
/// as its TerminalSession controller writes them: written before the runtime starts, they are the
/// launches its engine finds there.
pub fn runtime_terminals(daemon: &InProcessDaemon) -> flotilla_provisioning_testkit::TerminalsFixture {
    flotilla_provisioning_testkit::TerminalsFixture::over_service(&daemon.decided_store().expect("decided store"))
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
