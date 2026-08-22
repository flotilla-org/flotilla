use std::{cmp::Ordering, path::PathBuf};

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::{
    qualified_path::{qualified_path_or_host_path, QualifiedPath},
    EnvironmentId, HostName,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkout {
    pub branch: String,
    pub is_main: bool,
    pub trunk_ahead_behind: Option<AheadBehind>,
    pub remote_ahead_behind: Option<AheadBehind>,
    pub working_tree: Option<WorkingTreeStatus>,
    pub last_commit: Option<CommitInfo>,
    #[serde(default)]
    pub host_name: Option<HostName>,
    #[serde(default)]
    pub environment_id: Option<EnvironmentId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AheadBehind {
    pub ahead: i64,
    pub behind: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInfo {
    pub short_sha: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingTreeStatus {
    pub staged: usize,
    pub modified: usize,
    pub untracked: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRequest {
    pub title: String,
    pub branch: String,
    pub status: ChangeRequestStatus,
    pub body: Option<String>,
    #[serde(default)]
    pub provider_name: String,
    #[serde(default)]
    pub provider_display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeRequestStatus {
    Open,
    Draft,
    Merged,
    Closed,
}

impl std::fmt::Display for ChangeRequestStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open => write!(f, "open"),
            Self::Draft => write!(f, "draft"),
            Self::Merged => write!(f, "merged"),
            Self::Closed => write!(f, "closed"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct IssueSource {
    pub service: String,
    pub scope: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct IssueRef {
    pub source: IssueSource,
    pub id: String,
}

impl IssueRef {
    /// Compare issue references for the default issue-panel order: newest ID
    /// first. Numeric IDs sort before opaque IDs; each bucket sorts descending
    /// numerically or lexically. The source is a deterministic tie-breaker.
    pub fn cmp_id_desc(&self, other: &Self) -> Ordering {
        let by_id = match (self.id.parse::<u64>(), other.id.parse::<u64>()) {
            (Ok(left), Ok(right)) => right.cmp(&left),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => other.id.cmp(&self.id),
        };
        by_id.then_with(|| self.source.cmp(&other.source))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueState {
    Open,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct Issue {
    pub reference: IssueRef,
    pub title: String,
    pub body: Option<String>,
    pub state: IssueState,
    pub labels: Vec<String>,
    /// When the external source last changed the issue.
    pub as_of: DateTime<Utc>,
    /// When Flotilla observed this exact snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub provider_name: String,
    #[serde(default)]
    pub provider_display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueChangeset {
    pub updated: Vec<Issue>,
    pub closed: Vec<IssueRef>,
    /// Whether the provider had more changes than it returned. When true,
    /// the caller should discard this changeset and perform a full re-fetch
    /// instead of applying it incrementally. This differs from
    /// query result `has_more`, which signals additional pages to paginate.
    pub has_more: bool,
}

/// Which CLI tool / runtime is running the agent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentHarness {
    ClaudeCode,
    Codex,
    Gemini,
    OpenCode,
}

/// Fine-grained agent lifecycle status, richer than cloud session status.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentStatus {
    Idle,
    Active,
    WaitingForInput,
    WaitingForPermission,
    Errored,
}

/// Where the agent lives — local CLI process or cloud-provisioned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AgentContext {
    Local {
        attachable_id: AttachableId,
    },
    Cloud {
        provider_name: String,
        session_id: String,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        repo: Option<String>,
    },
}

/// A running coding agent — local CLI or cloud-provisioned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub harness: AgentHarness,
    pub status: AgentStatus,
    pub model: Option<String>,
    pub context: AgentContext,
    #[serde(default)]
    pub provider_name: String,
    #[serde(default)]
    pub provider_display_name: String,
    #[serde(default)]
    pub item_noun: String,
}

/// How a remote access point can be reached.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RemoteAccessType {
    Web,
    Ssh,
}

/// A remote access wrapper around an agent (e.g., Claude Code Web session).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteAccessPoint {
    pub provider_name: String,
    pub access_point_id: String,
    pub access_type: RemoteAccessType,
    pub url: Option<String>,
}

/// Normalized event types across all harnesses.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentEventType {
    Started,
    Ended,
    Active,
    Idle,
    WaitingForPermission,
    /// The event was informational and should not change agent status.
    NoChange,
}

impl AgentEventType {
    /// Returns the agent status this event implies, or None for NoChange.
    pub fn to_status(&self) -> Option<AgentStatus> {
        match self {
            AgentEventType::Started => Some(AgentStatus::Idle),
            AgentEventType::Ended => None, // caller should remove the entry
            AgentEventType::Active => Some(AgentStatus::Active),
            AgentEventType::Idle => Some(AgentStatus::Idle),
            AgentEventType::WaitingForPermission => Some(AgentStatus::WaitingForPermission),
            AgentEventType::NoChange => None,
        }
    }
}

/// A normalized agent hook event sent from the hook CLI to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct AgentHookEvent {
    /// Which terminal this agent lives in (from env or allocated).
    pub attachable_id: AttachableId,
    /// Which harness produced this event.
    pub harness: AgentHarness,
    /// What happened.
    pub event_type: AgentEventType,
    /// The agent's native session ID (if available).
    pub session_id: Option<String>,
    /// Model being used (if reported).
    pub model: Option<String>,
    /// Current working directory (if reported).
    pub cwd: Option<String>,
    /// Control-plane identity injected into managed crew sessions. Legacy
    /// observer hooks omit it and continue through their old path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<AgentHookTerminalRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHookTerminalRef {
    pub namespace: String,
    pub session_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudAgentSession {
    pub title: String,
    pub status: SessionStatus,
    pub model: Option<String>,
    pub updated_at: Option<String>,
    #[serde(default)]
    pub provider_name: String,
    #[serde(default)]
    pub provider_display_name: String,
    /// Capitalized item noun for this provider (e.g. "Agent", "Task").
    /// Lives in the protocol (not derived in the TUI) because the TUI may
    /// receive snapshots from a remote daemon and needs display context.
    #[serde(default)]
    pub item_noun: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    Running,
    Idle,
    Archived,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttachableSetId(String);

impl AttachableSetId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AttachableSetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttachableId(String);

impl AttachableId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AttachableId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachableSet {
    pub id: AttachableSetId,
    #[serde(default)]
    pub host_affinity: Option<HostName>,
    #[serde(default)]
    #[serde(with = "qualified_path_or_host_path::option")]
    pub checkout: Option<QualifiedPath>,
    #[serde(default)]
    pub template_identity: Option<String>,
    #[serde(default)]
    pub environment_id: Option<EnvironmentId>,
    #[serde(default)]
    pub members: Vec<AttachableId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalStatus {
    Running,
    Disconnected,
    Exited(i32),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneExitAttention {
    pub exit_code: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedTerminal {
    pub set_id: AttachableSetId,
    pub role: String,
    pub command: String,
    pub working_directory: PathBuf,
    pub status: TerminalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<PaneExitAttention>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub name: String,
    #[serde(default)]
    pub attachable_set_id: Option<AttachableSetId>,
}

/// All raw provider data for a single repo, keyed for lookup.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderData {
    #[serde(with = "crate::qualified_path::qualified_path_map")]
    pub checkouts: IndexMap<QualifiedPath, Checkout>,
    pub change_requests: IndexMap<String, ChangeRequest>,
    /// Legacy Plane-A snapshot for one repository. Keys are source-local IDs;
    /// never union this map across sources. Use each issue's canonical
    /// `Issue::reference` for project-level collections.
    pub issues: IndexMap<String, Issue>,
    pub sessions: IndexMap<String, CloudAgentSession>,
    pub branches: IndexMap<String, crate::delta::Branch>,
    pub workspaces: IndexMap<String, Workspace>,
    #[serde(default)]
    pub managed_terminals: IndexMap<AttachableId, ManagedTerminal>,
    pub attachable_sets: IndexMap<AttachableSetId, AttachableSet>,
    #[serde(default)]
    pub agents: IndexMap<String, Agent>,
}
