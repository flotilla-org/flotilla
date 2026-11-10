//! Crew operations, crew actions and capability cards, the leaf engine, the
//! decision log, charter notifications, and the Vessel, TerminalSession and
//! Convoy reconcilers.

pub mod charter_notifications;
pub mod convoy;
pub mod crew_actions;
pub mod crew_capabilities;
pub mod crew_ops;
pub mod decision_log;
pub mod leaf_engine;
mod message_port;
pub mod ports;
pub mod reconcilers;
pub mod review_bundle;

/// Readings that serve the engine's checkouts and vessels with none held, as a daemon's readings
/// always serve them (#3108). One store serves every unit test: it is only read.
#[cfg(test)]
pub(crate) fn no_held_checkouts() -> std::sync::Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads> {
    engine_fixture().reads()
}

#[cfg(test)]
fn engine_fixture() -> &'static flotilla_provisioning_testkit::EngineFixture {
    static FIXTURE: std::sync::OnceLock<flotilla_provisioning_testkit::EngineFixture> = std::sync::OnceLock::new();
    FIXTURE.get_or_init(flotilla_provisioning_testkit::EngineFixture::default)
}

/// Readings as [`no_held_checkouts`] serves them, also serving `agent_sessions`.
#[cfg(test)]
fn reads_with_agent_sessions(
    agent_sessions: std::sync::Arc<dyn flotilla_resources::agent_activity::AgentSessionReads>,
) -> std::sync::Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads> {
    use flotilla_provisioning::engine::reads::{LifecycleReadPort, StoreReads};
    let engine = engine_fixture();
    std::sync::Arc::new(
        LifecycleReadPort::new(StoreReads::new(std::sync::Arc::clone(&engine.checkouts.store)), engine.checkouts.home.clone())
            .with_checkouts(std::sync::Arc::clone(&engine.checkouts.checkouts) as _)
            .with_vessels(std::sync::Arc::clone(&engine.vessels.vessels) as _)
            .with_agent_sessions(agent_sessions),
    )
}

#[cfg(test)]
thread_local! {
    static AGENTS: std::cell::RefCell<Option<std::sync::Arc<flotilla_provisioning_testkit::AgentSessionsFixture>>> =
        const { std::cell::RefCell::new(None) };
}

/// Readings as [`no_held_checkouts`] serves them, also serving AgentSessions this test observes
/// through [`agents`] (#3108). Each test builds its readers and observes its agents on its own
/// thread, so the latest readings built on the thread are the test's.
#[cfg(test)]
pub(crate) fn agent_reads() -> std::sync::Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads> {
    let fixture =
        std::sync::Arc::new(flotilla_provisioning_testkit::AgentSessionsFixture::new(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH));
    AGENTS.with(|agents| *agents.borrow_mut() = Some(std::sync::Arc::clone(&fixture)));
    reads_with_agent_sessions(fixture)
}

/// Readings as a daemon restarted at `watched_from` serves them: this test's AgentSessions, with
/// none of their progress seen. Later observations through [`agents`] go to the restarted daemon.
#[cfg(test)]
pub(crate) fn restarted_agent_reads(
    watched_from: chrono::DateTime<chrono::Utc>,
) -> std::sync::Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads> {
    let fixture = std::sync::Arc::new(agents().restarted(watched_from));
    AGENTS.with(|agents| *agents.borrow_mut() = Some(std::sync::Arc::clone(&fixture)));
    reads_with_agent_sessions(fixture)
}

/// The AgentSessions the readings this test thread last built serve.
#[cfg(test)]
pub(crate) fn agents() -> std::sync::Arc<flotilla_provisioning_testkit::AgentSessionsFixture> {
    AGENTS.with(|agents| agents.borrow().clone()).expect("readings serving AgentSessions are built first")
}
