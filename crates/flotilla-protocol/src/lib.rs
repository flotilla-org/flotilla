pub mod arg;
pub mod commands;
pub mod delta;
pub mod environment;
pub mod framing;
mod host;
mod host_summary;
pub mod issue_query;
pub mod output;
pub mod path_context;
pub mod peer;
pub mod provider_data;
mod provisioning_target;
pub mod qualified_path;
pub mod query;
pub mod snapshot;
pub mod step;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use environment::{EnvironmentId, EnvironmentInfo, EnvironmentKind, EnvironmentSpec, EnvironmentStatus, ImageId, ImageSource};
pub use host::{HostName, HostPath, RepoIdentity};
pub use host_summary::{DiscoveryFact, HostEnvironment, HostProviderStatus, HostSnapshot, HostSummary, SystemInfo, ToolInventory};
pub use path_context::{DaemonHostPath, ExecutionEnvironmentPath};
pub use peer::{CommandPeerEvent, GoodbyeReason, PeerDataKind, PeerDataMessage, PeerWireMessage, RoutedPeerMessage, VectorClock};
pub use provisioning_target::ProvisioningTarget;
pub use step::{CheckoutIntent, Step, StepAction, StepExecutionContext, StepOutcome};

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
    CheckoutSelector, CheckoutStatus, CheckoutTarget, Command, CommandAction, CommandValue, PreparedTerminalCommand, PreparedWorkspace,
    RepoSelector, ResolvedPaneCommand, StepStatus,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionRole {
    Client,
    Peer,
}
pub use snapshot::{
    CategoryLabels, CheckoutRef, ProviderError, RepoInfo, RepoLabels, RepoSnapshot, WorkItem, WorkItemIdentity, WorkItemKind,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConfigLabel(pub String);

pub const PROTOCOL_VERSION: u32 = 5;

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
#[allow(clippy::large_enum_variant)]
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
    QueryResult { command_id: u64, value: commands::CommandValue },
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
        connection_role: Option<ConnectionRole>,
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
    RepoUntracked {
        repo_identity: RepoIdentity,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<std::path::PathBuf>,
    },
    #[serde(rename = "command_started")]
    CommandStarted {
        command_id: u64,
        host: HostName,
        repo_identity: RepoIdentity,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<std::path::PathBuf>,
        description: String,
    },
    #[serde(rename = "command_finished")]
    CommandFinished {
        command_id: u64,
        host: HostName,
        repo_identity: RepoIdentity,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<std::path::PathBuf>,
        result: commands::CommandValue,
    },
    #[serde(rename = "command_step_update")]
    CommandStepUpdate {
        command_id: u64,
        host: HostName,
        repo_identity: RepoIdentity,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<std::path::PathBuf>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<std::path::PathBuf>,
    pub changes: Vec<Change>,
    /// Pre-correlated work items from the daemon (avoids re-correlation on TUI side).
    pub work_items: Vec<snapshot::WorkItem>,
}

#[cfg(test)]
#[path = "lib/tests.rs"]
mod tests;
