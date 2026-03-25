pub mod arg;
pub mod commands;
pub mod delta;
pub mod environment;
pub mod framing;
mod host;
mod host_summary;
pub mod output;
pub mod path_context;
pub mod peer;
pub mod provider_data;
pub mod query;
pub mod snapshot;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use environment::{EnvironmentBinding, EnvironmentId, EnvironmentInfo, EnvironmentSpec, EnvironmentStatus, ImageId, ImageSource};
pub use host::{HostName, HostPath, RepoIdentity};
pub use host_summary::{DiscoveryFact, HostEnvironment, HostProviderStatus, HostSnapshot, HostSummary, SystemInfo, ToolInventory};
pub use path_context::{DaemonHostPath, ExecutionEnvironmentPath};
pub use peer::{CommandPeerEvent, GoodbyeReason, PeerDataKind, PeerDataMessage, PeerWireMessage, RoutedPeerMessage, VectorClock};

#[cfg(test)]
pub(crate) mod test_helpers {
    use serde::{de::DeserializeOwned, Serialize};

    /// Assert JSON roundtrip via re-serialization (for types without PartialEq).
    pub fn assert_json_roundtrip<T: Serialize + DeserializeOwned + std::fmt::Debug>(value: &T) {
        let json = serde_json::to_string(value).expect("serialize");
        let decoded: T = serde_json::from_str(&json).expect("deserialize");
        let json2 = serde_json::to_string(&decoded).expect("re-serialize");
        assert_eq!(json2, json, "JSON roundtrip mismatch");
    }

    /// Assert JSON roundtrip via PartialEq (for types that derive it).
    pub fn assert_roundtrip<T: Serialize + DeserializeOwned + std::fmt::Debug + PartialEq>(value: &T) {
        let json = serde_json::to_string(value).expect("serialize");
        let decoded: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, *value);
    }
}

pub use commands::{
    CheckoutSelector, CheckoutStatus, CheckoutTarget, Command, CommandAction, CommandValue, PreparedTerminalCommand, RepoSelector,
    ResolvedPaneCommand, StepStatus,
};
pub use delta::{Branch, BranchStatus, Change, DeltaEntry, EntryOp};
pub use provider_data::{
    Agent, AgentContext, AgentEventType, AgentHarness, AgentHookEvent, AgentStatus, AheadBehind, AssociationKey, AttachableId,
    AttachableSet, AttachableSetId, ChangeRequest, ChangeRequestStatus, Checkout, CloudAgentSession, CommitInfo, CorrelationKey, Issue,
    IssueChangeset, IssuePage, ManagedTerminal, ProviderData, RemoteAccessPoint, RemoteAccessType, SessionStatus, TerminalStatus,
    WorkingTreeStatus, Workspace,
};
pub use query::{
    DiscoveryEntry, HostListEntry, HostListResponse, HostProvidersResponse, HostStatusResponse, ProviderHealthMap, ProviderInfo,
    RepoDetailResponse, RepoProvidersResponse, RepoSummary, RepoWorkResponse, StatusResponse, TopologyResponse, TopologyRoute,
    UnmetRequirementInfo,
};
use serde::{Deserialize, Serialize};
pub use snapshot::{
    CategoryLabels, CheckoutRef, ProviderError, RepoInfo, RepoLabels, RepoSnapshot, WorkItem, WorkItemIdentity, WorkItemKind,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConfigLabel(pub String);

pub const PROTOCOL_VERSION: u32 = 4;

/// Key for identifying an event stream in replay cursors.
/// Each stream has its own independent sequence counter.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum StreamKey {
    #[serde(rename = "repo")]
    Repo { identity: RepoIdentity },
    #[serde(rename = "host")]
    Host { host_name: HostName },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayCursor {
    pub stream: StreamKey,
    pub seq: u64,
}

/// Typed client-to-daemon RPC requests.
///
/// On the wire, the tagged enum payload is encoded under `params`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "params", rename_all = "snake_case")]
pub enum Request {
    ListRepos,
    GetState { repo: std::path::PathBuf },
    Execute { command: Command },
    Cancel { command_id: u64 },
    Refresh { repo: std::path::PathBuf },
    AddRepo { path: std::path::PathBuf },
    RemoveRepo { path: std::path::PathBuf },
    ReplaySince { last_seen: Vec<ReplayCursor> },
    GetStatus,
    GetTopology,
    AgentHook { event: AgentHookEvent },
}

/// Typed daemon RPC success payloads.
///
/// On the wire, the tagged enum payload is encoded under `data`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Response {
    ListRepos(Vec<RepoInfo>),
    GetState(Box<RepoSnapshot>),
    Execute { command_id: u64 },
    Cancel,
    Refresh,
    AddRepo,
    RemoveRepo,
    ReplaySince(Vec<DaemonEvent>),
    GetStatus(StatusResponse),
    GetTopology(TopologyResponse),
    AgentHook,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResponseResult {
    Ok { response: Box<Response> },
    Err { message: String },
}

/// Top-level message envelope for the JSON protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Message {
    #[serde(rename = "request")]
    Request { id: u64, request: Request },
    #[serde(rename = "response")]
    Response { id: u64, response: Box<ResponseResult> },
    #[serde(rename = "event")]
    Event { event: Box<DaemonEvent> },
    #[serde(rename = "hello")]
    Hello {
        protocol_version: u32,
        host_name: HostName,
        #[serde(default = "uuid::Uuid::nil")]
        session_id: uuid::Uuid,
        #[serde(default)]
        environment_id: Option<EnvironmentId>,
    },
    #[serde(rename = "peer")]
    Peer(Box<PeerWireMessage>),
}

impl Message {
    /// Build a success response.
    pub fn ok_response(id: u64, response: Response) -> Self {
        Message::Response { id, response: Box::new(ResponseResult::Ok { response: Box::new(response) }) }
    }

    /// Build an error response.
    pub fn error_response(id: u64, message: impl Into<String>) -> Self {
        Message::Response { id, response: Box::new(ResponseResult::Err { message: message.into() }) }
    }
}

/// Events pushed from daemon to subscribed clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum DaemonEvent {
    /// Full snapshot — sent on initial connect, after seq gaps, or when delta
    /// would be larger than the full snapshot.
    #[serde(rename = "repo_snapshot")]
    RepoSnapshot(Box<RepoSnapshot>),
    /// Incremental delta — sent when only a subset of data changed.
    #[serde(rename = "repo_delta")]
    RepoDelta(Box<RepoDelta>),
    #[serde(rename = "repo_tracked")]
    RepoTracked(Box<RepoInfo>),
    #[serde(rename = "repo_untracked")]
    RepoUntracked { repo_identity: RepoIdentity, path: std::path::PathBuf },
    #[serde(rename = "command_started")]
    CommandStarted { command_id: u64, host: HostName, repo_identity: RepoIdentity, repo: std::path::PathBuf, description: String },
    #[serde(rename = "command_finished")]
    CommandFinished {
        command_id: u64,
        host: HostName,
        repo_identity: RepoIdentity,
        repo: std::path::PathBuf,
        result: commands::CommandValue,
    },
    #[serde(rename = "command_step_update")]
    CommandStepUpdate {
        command_id: u64,
        host: HostName,
        repo_identity: RepoIdentity,
        repo: std::path::PathBuf,
        step_index: usize,
        step_count: usize,
        description: String,
        status: commands::StepStatus,
    },
    /// A peer host's connection status changed.
    #[serde(rename = "peer_status")]
    PeerStatusChanged { host: HostName, status: PeerConnectionState },
    /// Full host snapshot — sent on initial connect/replay and when
    /// a host's summary or connection status changes.
    #[serde(rename = "host_snapshot")]
    HostSnapshot(Box<HostSnapshot>),
    /// Host stream tombstone — sent when a previously visible host disappears.
    #[serde(rename = "host_removed")]
    HostRemoved { host: HostName, seq: u64 },
}

/// Peer connection state as seen by the TUI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerConnectionState {
    Connected,
    Disconnected,
    Connecting,
    Reconnecting,
    Rejected { reason: String },
}

/// A delta update for a repo snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoDelta {
    pub seq: u64,
    pub prev_seq: u64,
    pub repo_identity: RepoIdentity,
    pub repo: std::path::PathBuf,
    pub changes: Vec<Change>,
    /// Pre-correlated work items from the daemon (avoids re-correlation on TUI side).
    pub work_items: Vec<snapshot::WorkItem>,
    /// Issue metadata (not part of delta log, but needed by TUI).
    pub issue_total: Option<u32>,
    pub issue_has_more: bool,
    pub issue_search_results: Option<Vec<(String, Issue)>>,
}

#[cfg(test)]
#[path = "lib/tests.rs"]
mod tests;
