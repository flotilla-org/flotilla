use async_trait::async_trait;
use flotilla_protocol::{EnvironmentInfo, ToolInventory};
#[async_trait]
pub trait EnvironmentSummaries: Send + Sync {
    fn local_inventory(&self) -> ToolInventory;
    async fn host_summary_environments(&self) -> Vec<EnvironmentInfo>;
    async fn visible_environments(&self) -> Vec<EnvironmentInfo>;
}

use flotilla_protocol::{AttachBinding, ResolvedAttachPlan};
/// An attach resolution: the plan the CLI should execute, plus the
/// structured binding it stamps onto its enclosing PM pane (#708).
#[derive(Debug, Clone)]
pub struct ResolvedAttach {
    pub plan: ResolvedAttachPlan,
    pub binding: Option<AttachBinding>,
}

pub use flotilla_resources::lifecycle_reads::{LateLifecycleReads, LifecycleReads, NoLifecycleReads, ProvisioningKind};

use flotilla_protocol::{AgentSessionReading, NodeId};
use futures::stream::BoxStream;

/// An AgentSession's identity: its home, and its name, which is the name of the
/// TerminalSession record it runs in.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentSessionKey {
    pub home: NodeId,
    pub name: String,
}

/// What changed among the AgentSessions the store holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSessionUpdate {
    /// These sessions were written: each with its reading now, or `None` once it is gone.
    Changed(Vec<(AgentSessionKey, Option<AgentSessionReading>)>),
    /// Every session held for `home` now, replacing what was held for it.
    Home { home: NodeId, sessions: Vec<(String, AgentSessionReading)> },
    /// This home last heard from `home` at `at`: its readings are current until
    /// [`flotilla_resources::agent_activity::HOME_HEARD_WITHIN`] after.
    Heard { home: NodeId, at: chrono::DateTime<chrono::Utc> },
}

/// The AgentSessions held when following began, and every update after them.
pub struct AgentSessionFollow {
    /// The home the store writes; this daemon's own sessions carry it.
    pub home: NodeId,
    pub held: Vec<(AgentSessionKey, AgentSessionReading)>,
    /// When this home last heard from each other home it follows; one never heard from is
    /// absent. This home's own sessions are always current.
    pub heard: Vec<(NodeId, chrono::DateTime<chrono::Utc>)>,
    pub updates: BoxStream<'static, AgentSessionUpdate>,
}

/// The AgentSessions in the local decided store (#3108): this home's, and the replicas of
/// every followed home's. Updates arrive as the store commits, local writes and other homes'
/// entries alike; no timer reads them.
#[async_trait]
pub trait AgentSessions: Send + Sync {
    /// Starts following before reading what is held, so no write between the two is missed.
    /// Unavailable where no decided store is composed.
    async fn follow(&self) -> Result<AgentSessionFollow, String>;
}

/// No decided store: no crew member shows an AgentSession.
pub struct NoAgentSessions;

#[async_trait]
impl AgentSessions for NoAgentSessions {
    async fn follow(&self) -> Result<AgentSessionFollow, String> {
        Err("AgentSessions are not served without the decided store".into())
    }
}
