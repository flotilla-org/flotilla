//! PROPOSAL for the owner's schema look-over (#3108): the records a TerminalSession launch needs
//! beside its request once the engine is its single writer. Nothing registers these in a
//! production registry and nothing reads or writes them; [`super::terminal_proposal_registry`]
//! exists only so their DDL and view can be shown.

use flotilla_schema::{Records, Stored, Timestamp};

use super::launch_facts::LaunchLimits;

/// What one launch of a TerminalSession was given, fixed when it was realised: kind code `tf`, a
/// plain record named and homed as the launch it describes (`name#n` at its environment's
/// host). The TerminalSession controller writes it in the transaction of that launch's Realise
/// edge and nothing updates it; the next launch is a new TerminalSession record and gets its
/// own. It archives and expunges with its launch.
///
/// Not here: the pid (no reader), the launch time (the Realise edge's time), the session id (the
/// realised identity), attention, screen and output samples (judged or sampled, never stored),
/// and the crew id: a launch's AgentSession is named as the launch, so `FLOTILLA_CREW_ID` is the
/// launch name and nothing looks a launch up by another id.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "TerminalLaunchFacts", code = "tf")]
pub struct TerminalLaunchFacts {
    /// The command line the pool started, for attach.
    pub launch_command: String,
    /// The cleat daemon holding the session, when the pool is cleat. A cleat session lives and
    /// dies with its daemon, so this is fixed for the launch.
    #[store(flatten)]
    pub endpoint: Option<TerminalEndpoint>,
    /// The harness the capability resolved to, for an agent's launch.
    #[store(flatten)]
    pub agent: Option<AgentLaunch>,
    /// The limits the launch was configured with; each absent one is unknown, not unlimited.
    #[store(flatten)]
    pub configured_limits: LaunchLimits,
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

/// Whether a realised environment launch can take work: kind code `eh`, a plain record named and
/// homed as the Environment launch it describes. Its one writer is that environment's probe at
/// its home, and it writes only when the value flips, so each commit is an edge fact: a
/// TerminalSession launch that failed retryably against an unusable environment names this
/// record as its usability fact and is woken by the flip, with no timer. The reason and every
/// sample stay in logs and metrics (ruling Q2): presence alone is not an ending; only absence
/// ends the environment (Vanish Lost).
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "EnvironmentHealth", code = "eh")]
pub struct EnvironmentHealth {
    pub usable: bool,
}

/// Composer text found in a launch's terminal and set aside before a turn was typed over it:
/// kind code `td`, a plain record named and homed as the launch. Its one writer is the terminal
/// transport at the launch's home; rows are only appended.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
#[store(kind = "TerminalDrafts", code = "td")]
pub struct TerminalDrafts {
    #[store(records)]
    pub drafts: Records<StashedDraft>,
}

/// One draft set aside.
#[derive(Clone, Debug, PartialEq, Eq, Stored)]
pub struct StashedDraft {
    pub text: String,
    /// The message whose delivery found it.
    pub message: Option<String>,
    pub stashed_at: Timestamp,
}
