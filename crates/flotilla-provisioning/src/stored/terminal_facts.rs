//! The record a TerminalSession launch needs beside its request (#3108, owner's look-over): what
//! the launch was given when it was realised.
//!
//! Not stored, by the owner's rulings: composer drafts (the terminal pool holds set-aside text in
//! memory), environment usability (the TerminalSession controller reads it from the environment's
//! record, as the Clone controller reads its environment's), and progress, liveness and screen
//! readings (in memory at the adapter, traced).

use flotilla_schema::Stored;

/// What one launch of a TerminalSession was given, fixed when it was realised: kind code `tf`, a
/// plain record named and homed as the launch it describes (`name#n` at its environment's
/// host). The TerminalSession controller writes it in the transaction of that launch's Realise
/// edge and nothing updates it; the next launch is a new TerminalSession record and gets its
/// own. It archives and expunges with its launch.
///
/// Each column has a reader at another daemon. Not here: the launch command (only the session's
/// own daemon reads it, to attach a passthrough session), the pid (no reader), the launch and
/// stop times (the Realise and ending edges' times), the session id (the realised identity), the
/// process's exit (the ending edge's cause), the CPU limits (never set for a terminal launch),
/// and the crew id: a launch's AgentSession is named as the launch, so `FLOTILLA_CREW_ID` is the
/// launch name and nothing looks a launch up by another id.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "TerminalLaunchFacts", code = "tf")]
pub struct TerminalLaunchFacts {
    /// The cleat daemon holding the session, when the pool is cleat: what a direct attach from
    /// another machine connects to. A cleat session lives and dies with its daemon, so this is
    /// fixed for the launch.
    #[store(flatten)]
    pub endpoint: Option<TerminalEndpoint>,
    /// The harness the capability resolved to, for an agent's launch: crew list, the stall
    /// advisory and Codex turn confirmation read it.
    #[store(flatten)]
    pub agent: Option<AgentLaunch>,
    /// The Rust build and linker parallelism a host-direct launch was given, shown on its vessel;
    /// absent for a launch that was given none.
    pub build_jobs: Option<u64>,
}

/// Where a cleat session runs.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct TerminalEndpoint {
    pub runtime_root: String,
    pub daemon: String,
}

/// The harness an agent launch resolved to.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct AgentLaunch {
    #[store(intern = "ad")]
    pub adapter: String,
    pub model: Option<String>,
    /// The harness's permission stance, as its adapter names it.
    pub stance: String,
}
