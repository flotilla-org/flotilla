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
    machines::{
        convoy::Convoy,
        lifecycle::{Lifecycle, LifecycleKind, LifecycleState},
        vessel::Vessel,
    },
    Resource,
};

use super::environment::Environment;

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
/// Realised under the terminal pool's session id once it is observed running.
/// Its pid, endpoint, launch command, screen, attention and occupancy are
/// observations. The old spec's queued crew messages are Messages (#3317),
/// never part of the request, and a pending crew completion is a Message to
/// the convoy's home.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(
    kind = "TerminalSession",
    code = "ts",
    state = LifecycleState,
    index(name = "by_environment", eq(environment)),
    index(name = "by_vessel", eq(vessel), when(vessel is_some)),
    index(name = "by_convoy", eq(convoy), when(convoy is_some))
)]
pub struct TerminalSessionRequest {
    pub environment: Ref<Resource<Environment>>,
    /// The vessel it runs in, for a crew's sessions.
    pub vessel: Option<Ref<Resource<Vessel>>>,
    /// The convoy whose crew an agent session runs: the vessel's convoy when
    /// both are set, which the writer checks. Its own column so that a
    /// convoy's sessions are one indexed read.
    pub convoy: Option<Ref<Resource<Convoy>>>,
    #[store(intern = "rl")]
    pub role: String,
    /// The terminal pool, interned (domain `tp`).
    #[store(intern = "tp")]
    pub pool: String,
    pub cwd: String,
    /// Declared, non-secret launch values, replicated with the session.
    /// Credentials are delivered at launch and never stored here; they and
    /// adapter requirements take precedence.
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
