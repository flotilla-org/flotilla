//! TerminalSession: one process in one environment's terminal pool, a crew's
//! agent or a tool (#3629 Track B layer 5).
//!
//! Declared here, beside Environment, rather than beside its reconciler:
//! `flotilla-crew` must not reach the engine, and a session refers to its
//! Environment by `Ref`, which only a crate holding that kind can name.

use std::collections::BTreeMap;

use flotilla_resources::TerminalBrief;
use flotilla_schema::{Frozen, Ref, Stored};
use flotilla_state_machines::{
    machines::lifecycle::{Lifecycle, LifecycleKind, LifecycleState},
    Resource,
};

use super::{environment::Environment, Holder};

/// Terminal sessions as a lifecycle kind.
pub struct TerminalSessionKind;

impl LifecycleKind for TerminalSessionKind {
    const NAME: &'static str = "TerminalSession";
    type Request = TerminalSessionRequest;
}

/// The TerminalSession machine: the shared lifecycle over its request.
pub type TerminalSession = Lifecycle<TerminalSessionKind>;

/// What a TerminalSession launches, where. It declares the stored
/// TerminalSession (kind code `ts`), homed at its environment's host.
///
/// Realised under the terminal pool's session id once it is observed running;
/// that id is not indexed (nothing looks a session up by it). Its pid, endpoint, launch command, screen, attention and occupancy are
/// observations. The old spec's queued crew messages are Messages (#3317),
/// never part of the request, and a pending crew completion is a Message to
/// the convoy's home.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(
    kind = "TerminalSession",
    code = "ts",
    state = LifecycleState,
    index(name = "by_environment", eq(environment)),
    index(name = "by_holder", eq(holder), when(holder is_some))
)]
pub struct TerminalSessionRequest {
    pub environment: Ref<Resource<Environment>>,
    /// The vessel it is held for (`vessel:<name>`), for a crew's session;
    /// none for a standalone tool session (owner's look-over of #3720,
    /// 2026-10-22). Its convoy is the vessel's: a convoy's sessions are its
    /// vessels by convoy, then their sessions by holder.
    pub holder: Option<Holder>,
    #[store(intern = "rl")]
    pub role: String,
    /// The terminal pool, interned (domain `tp`).
    #[store(intern = "tp")]
    pub pool: String,
    pub cwd: String,
    /// Declared, non-secret launch values, replicated with the session.
    /// Credentials are delivered at launch and never stored here. At launch,
    /// credentials and adapter requirements override these values.
    #[store(child)]
    pub env: BTreeMap<String, String>,
    pub source: Launch,
}

/// What the session runs.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub enum Launch {
    /// A tool's command line.
    #[store(code = "tl")]
    Tool { command: String },
    /// An agent harness chosen by capability, started with its brief.
    #[store(code = "ag")]
    Agent {
        #[store(intern = "cp")]
        capability: String,
        adapter: Option<String>,
        model: Option<String>,
        /// The launch brief: its path, body and copies, stored once by content hash.
        #[store(frozen)]
        brief: Frozen<TerminalBrief>,
    },
}
