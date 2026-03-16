pub mod commands;
pub mod delta;
pub mod framing;
mod host;
mod host_summary;
pub mod output;
pub mod peer;
pub mod provider_data;
pub mod query;
pub mod snapshot;

pub use host::{HostName, HostPath, RepoIdentity};
pub use host_summary::{DiscoveryFact, HostEnvironment, HostProviderStatus, HostSummary, SystemInfo, ToolInventory};
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
    CheckoutSelector, CheckoutStatus, CheckoutTarget, Command, CommandAction, CommandResult, PreparedTerminalCommand, RepoSelector,
    StepStatus,
};
pub use delta::{Branch, BranchStatus, Change, DeltaEntry, EntryOp};
pub use provider_data::{
    AheadBehind, AssociationKey, ChangeRequest, ChangeRequestStatus, Checkout, CloudAgentSession, CommitInfo, CorrelationKey, Issue,
    IssueChangeset, IssuePage, ManagedTerminal, ManagedTerminalId, ProviderData, SessionStatus, TerminalStatus, WorkingTreeStatus,
    Workspace,
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

pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayCursor {
    pub repo_identity: RepoIdentity,
    pub seq: u64,
}

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
    GetRepoDetail { slug: String },
    GetRepoProviders { slug: String },
    GetRepoWork { slug: String },
    ListHosts,
    GetHostStatus { host: String },
    GetHostProviders { host: String },
    GetTopology,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Response {
    ListRepos(Vec<RepoInfo>),
    GetState(RepoSnapshot),
    Execute { command_id: u64 },
    Cancel,
    Refresh,
    AddRepo,
    RemoveRepo,
    ReplaySince(Vec<DaemonEvent>),
    GetStatus(StatusResponse),
    GetRepoDetail(RepoDetailResponse),
    GetRepoProviders(RepoProvidersResponse),
    GetRepoWork(RepoWorkResponse),
    ListHosts(HostListResponse),
    GetHostStatus(HostStatusResponse),
    GetHostProviders(HostProvidersResponse),
    GetTopology(TopologyResponse),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResponseResult {
    Ok { response: Response },
    Err { message: String },
}

/// Top-level message envelope for the JSON protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Message {
    #[serde(rename = "request")]
    Request { id: u64, request: Request },
    #[serde(rename = "response")]
    Response { id: u64, response: ResponseResult },
    #[serde(rename = "event")]
    Event { event: Box<DaemonEvent> },
    #[serde(rename = "hello")]
    Hello {
        protocol_version: u32,
        host_name: HostName,
        #[serde(default = "uuid::Uuid::nil")]
        session_id: uuid::Uuid,
    },
    #[serde(rename = "peer")]
    Peer(Box<PeerWireMessage>),
}

impl Message {
    /// Build a success response.
    pub fn ok_response(id: u64, response: Response) -> Self {
        Message::Response { id, response: ResponseResult::Ok { response } }
    }

    /// Build an error response.
    pub fn error_response(id: u64, message: impl Into<String>) -> Self {
        Message::Response { id, response: ResponseResult::Err { message: message.into() } }
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
    #[serde(rename = "repo_added")]
    RepoAdded(Box<RepoInfo>),
    #[serde(rename = "repo_removed")]
    RepoRemoved { repo_identity: RepoIdentity, path: std::path::PathBuf },
    #[serde(rename = "command_started")]
    CommandStarted { command_id: u64, host: HostName, repo_identity: RepoIdentity, repo: std::path::PathBuf, description: String },
    #[serde(rename = "command_finished")]
    CommandFinished {
        command_id: u64,
        host: HostName,
        repo_identity: RepoIdentity,
        repo: std::path::PathBuf,
        result: commands::CommandResult,
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
mod tests {
    use std::{collections::HashMap, path::PathBuf};

    use super::*;

    fn hp(path: &str) -> HostPath {
        HostPath::new(HostName::new("test-host"), PathBuf::from(path))
    }

    fn sample_command() -> Command {
        Command { host: None, context_repo: None, action: CommandAction::AddRepo { path: PathBuf::from("/tmp/my-repo") } }
    }

    #[test]
    fn message_request_roundtrip() {
        let msg = Message::Request { id: 42, request: Request::GetState { repo: PathBuf::from("/tmp/my-repo") } };
        let json = serde_json::to_string(&msg).expect("serialize");
        let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
        match deserialized {
            Message::Request { id, request } => {
                assert_eq!(id, 42);
                assert_eq!(request, Request::GetState { repo: PathBuf::from("/tmp/my-repo") });
            }
            other => panic!("expected Request, got {:?}", other),
        }
    }

    #[test]
    fn message_response_roundtrip() {
        let msg = Message::Response { id: 1, response: ResponseResult::Ok { response: Response::ListRepos(vec![]) } };
        let json = serde_json::to_string(&msg).expect("serialize");
        let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
        match deserialized {
            Message::Response { id, response } => {
                assert_eq!(id, 1);
                match response {
                    ResponseResult::Ok { response: Response::ListRepos(repos) } => assert!(repos.is_empty()),
                    other => panic!("expected list repos response, got {:?}", other),
                }
            }
            other => panic!("expected Response, got {:?}", other),
        }

        let msg = Message::Response { id: 2, response: ResponseResult::Ok { response: Response::Execute { command_id: 99 } } };
        let json = serde_json::to_string(&msg).expect("serialize");
        let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
        match deserialized {
            Message::Response { id, response } => {
                assert_eq!(id, 2);
                match response {
                    ResponseResult::Ok { response: Response::Execute { command_id } } => assert_eq!(command_id, 99),
                    other => panic!("expected execute response, got {:?}", other),
                }
            }
            other => panic!("expected Response, got {:?}", other),
        }

        let msg = Message::Response { id: 3, response: ResponseResult::Err { message: "not found".to_string() } };
        let json = serde_json::to_string(&msg).expect("serialize");
        let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
        match deserialized {
            Message::Response { id, response } => {
                assert_eq!(id, 3);
                match response {
                    ResponseResult::Err { message } => assert_eq!(message, "not found"),
                    other => panic!("expected error response, got {:?}", other),
                }
            }
            other => panic!("expected Response, got {:?}", other),
        }
    }

    #[test]
    fn message_request_roundtrip_covers_unit_and_command_variants() {
        let list_repos = Message::Request { id: 7, request: Request::ListRepos };
        let json = serde_json::to_string(&list_repos).expect("serialize");
        let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
        match deserialized {
            Message::Request { id, request } => {
                assert_eq!(id, 7);
                assert_eq!(request, Request::ListRepos);
            }
            other => panic!("expected Request, got {:?}", other),
        }

        let execute = Message::Request { id: 9, request: Request::Execute { command: sample_command() } };
        let json = serde_json::to_string(&execute).expect("serialize");
        let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
        match deserialized {
            Message::Request { id, request } => {
                assert_eq!(id, 9);
                assert_eq!(request, Request::Execute { command: sample_command() });
            }
            other => panic!("expected Request, got {:?}", other),
        }
    }

    #[test]
    fn message_event_snapshot_roundtrip() {
        let snapshot = RepoSnapshot {
            seq: 7,
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/my-repo".into() },
            repo: PathBuf::from("/tmp/my-repo"),
            host_name: HostName::new("test-host"),
            work_items: vec![WorkItem {
                kind: WorkItemKind::Checkout,
                identity: WorkItemIdentity::Checkout(hp("/tmp/my-repo/wt")),
                host: HostName::new("test-host"),
                branch: Some("feature-x".to_string()),
                description: "Feature X".to_string(),
                checkout: Some(CheckoutRef { key: hp("/tmp/my-repo/wt"), is_main_checkout: false }),
                change_request_key: Some("PR#10".to_string()),
                session_key: None,
                issue_keys: vec!["ISSUE-1".to_string()],
                workspace_refs: vec![],
                is_main_checkout: false,
                debug_group: vec![],
                source: None,
                terminal_keys: vec![],
            }],
            providers: ProviderData::default(),
            provider_health: HashMap::from([
                ("vcs".to_string(), HashMap::from([("Git".to_string(), true)])),
                ("change_request".to_string(), HashMap::from([("GitHub".to_string(), false)])),
            ]),
            errors: vec![ProviderError { category: "github".to_string(), provider: String::new(), message: "rate limited".to_string() }],
            issue_total: None,
            issue_has_more: false,
            issue_search_results: None,
        };
        let msg = Message::Event { event: Box::new(DaemonEvent::RepoSnapshot(Box::new(snapshot))) };
        let json = serde_json::to_string(&msg).expect("serialize");
        let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
        match deserialized {
            Message::Event { event } => match *event {
                DaemonEvent::RepoSnapshot(snap) => {
                    let snap = *snap;
                    assert_eq!(snap.seq, 7);
                    assert_eq!(snap.repo_identity, RepoIdentity { authority: "github.com".into(), path: "owner/my-repo".into() });
                    assert_eq!(snap.repo, PathBuf::from("/tmp/my-repo"));
                    assert_eq!(snap.work_items.len(), 1);
                    assert_eq!(snap.work_items[0].branch.as_deref(), Some("feature-x"));
                    assert_eq!(snap.work_items[0].kind, WorkItemKind::Checkout);
                    assert!(snap.provider_health["vcs"]["Git"]);
                    assert!(!snap.provider_health["change_request"]["GitHub"]);
                    assert_eq!(snap.errors.len(), 1);
                    assert_eq!(snap.errors[0].category, "github");
                }
                other => panic!("expected RepoSnapshot event, got {:?}", other),
            },
            other => panic!("expected Event, got {:?}", other),
        }
    }

    #[test]
    fn snapshot_delta_event_roundtrip() {
        let delta = RepoDelta {
            seq: 3,
            prev_seq: 2,
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/my-repo".into() },
            repo: PathBuf::from("/tmp/my-repo"),
            changes: vec![
                Change::Branch { key: "feat-x".into(), op: EntryOp::Added(Branch { status: BranchStatus::Remote }) },
                Change::Issue { key: "42".into(), op: EntryOp::Removed },
            ],
            work_items: vec![],
            issue_total: Some(100),
            issue_has_more: true,
            issue_search_results: None,
        };
        let msg = Message::Event { event: Box::new(DaemonEvent::RepoDelta(Box::new(delta))) };
        let json = serde_json::to_string(&msg).expect("serialize");
        let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
        match deserialized {
            Message::Event { event } => match *event {
                DaemonEvent::RepoDelta(d) => {
                    assert_eq!(d.seq, 3);
                    assert_eq!(d.prev_seq, 2);
                    assert_eq!(d.repo_identity, RepoIdentity { authority: "github.com".into(), path: "owner/my-repo".into() });
                    assert_eq!(d.repo, PathBuf::from("/tmp/my-repo"));
                    assert_eq!(d.changes.len(), 2);
                    assert_eq!(d.issue_total, Some(100));
                    assert!(d.issue_has_more);
                    assert!(d.issue_search_results.is_none());
                }
                other => panic!("expected RepoDelta, got {:?}", other),
            },
            other => panic!("expected Event, got {:?}", other),
        }
    }

    #[test]
    fn ok_response_builds_with_serialized_data() {
        let msg = Message::ok_response(7, Response::Execute { command_id: 42 });
        match msg {
            Message::Response { id, response } => {
                assert_eq!(id, 7);
                match response {
                    ResponseResult::Ok { response: Response::Execute { command_id } } => assert_eq!(command_id, 42),
                    other => panic!("expected execute response, got {:?}", other),
                }
            }
            other => panic!("expected Response, got {:?}", other),
        }
    }

    #[test]
    fn ok_response_builds_with_unit_variant() {
        let msg = Message::ok_response(99, Response::Refresh);
        match msg {
            Message::Response { id, response } => {
                assert_eq!(id, 99);
                match response {
                    ResponseResult::Ok { response: Response::Refresh } => {}
                    other => panic!("expected refresh response, got {:?}", other),
                }
            }
            other => panic!("expected Response, got {:?}", other),
        }
    }

    #[test]
    fn error_response_builds_with_error_message() {
        let msg = Message::error_response(5, "something went wrong");
        match msg {
            Message::Response { id, response } => {
                assert_eq!(id, 5);
                match response {
                    ResponseResult::Err { message } => assert_eq!(message, "something went wrong"),
                    other => panic!("expected error response, got {:?}", other),
                }
            }
            other => panic!("expected Response, got {:?}", other),
        }
    }

    #[test]
    fn daemon_event_command_started_roundtrip() {
        let event = DaemonEvent::CommandStarted {
            command_id: 42,
            host: HostName::new("desktop"),
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            repo: PathBuf::from("/tmp/repo"),
            description: "Creating checkout...".to_string(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let decoded: DaemonEvent = serde_json::from_str(&json).expect("deserialize");
        match decoded {
            DaemonEvent::CommandStarted { command_id, host, repo_identity, repo, description } => {
                assert_eq!(command_id, 42);
                assert_eq!(host, HostName::new("desktop"));
                assert_eq!(repo_identity, RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() });
                assert_eq!(repo, PathBuf::from("/tmp/repo"));
                assert_eq!(description, "Creating checkout...");
            }
            other => panic!("expected CommandStarted, got {:?}", other),
        }
    }

    #[test]
    fn daemon_event_command_finished_roundtrip() {
        let event = DaemonEvent::CommandFinished {
            command_id: 42,
            host: HostName::new("desktop"),
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            repo: PathBuf::from("/tmp/repo"),
            result: CommandResult::CheckoutCreated { branch: "feat-x".into(), path: PathBuf::from("/tmp/repo/feat-x") },
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let decoded: DaemonEvent = serde_json::from_str(&json).expect("deserialize");
        match decoded {
            DaemonEvent::CommandFinished { command_id, host, repo_identity, repo, result } => {
                assert_eq!(command_id, 42);
                assert_eq!(host, HostName::new("desktop"));
                assert_eq!(repo_identity, RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() });
                assert_eq!(repo, PathBuf::from("/tmp/repo"));
                match result {
                    CommandResult::CheckoutCreated { branch, path } => {
                        assert_eq!(branch, "feat-x");
                        assert_eq!(path, PathBuf::from("/tmp/repo/feat-x"));
                    }
                    other => panic!("expected CheckoutCreated, got {:?}", other),
                }
            }
            other => panic!("expected CommandFinished, got {:?}", other),
        }
    }

    #[test]
    fn snapshot_delta_roundtrip_preserves_repo_identity() {
        let delta = RepoDelta {
            seq: 2,
            prev_seq: 1,
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            repo: PathBuf::from("/tmp/repo"),
            changes: vec![],
            work_items: vec![],
            issue_total: Some(12),
            issue_has_more: true,
            issue_search_results: None,
        };

        let json = serde_json::to_string(&delta).expect("serialize");
        let decoded: RepoDelta = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.repo_identity, RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() });
        assert_eq!(decoded.repo, PathBuf::from("/tmp/repo"));
    }

    #[test]
    fn replay_cursor_roundtrip_preserves_repo_identity() {
        let cursor = ReplayCursor { repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() }, seq: 42 };
        test_helpers::assert_roundtrip(&cursor);
    }

    #[test]
    fn message_hello_roundtrip() {
        let msg = Message::Hello { protocol_version: 2, host_name: HostName::new("desktop"), session_id: uuid::Uuid::nil() };

        test_helpers::assert_json_roundtrip(&msg);
    }

    #[test]
    fn message_peer_data_roundtrip() {
        let msg = Message::Peer(Box::new(PeerWireMessage::Data(PeerDataMessage {
            origin_host: HostName::new("desktop"),
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            repo_path: PathBuf::from("/tmp/repo"),
            clock: VectorClock::default(),
            kind: PeerDataKind::Snapshot { data: Box::new(ProviderData::default()), seq: 7 },
        })));

        test_helpers::assert_json_roundtrip(&msg);
    }

    #[test]
    fn message_peer_routed_request_resync_roundtrip() {
        let msg = Message::Peer(Box::new(PeerWireMessage::Routed(RoutedPeerMessage::RequestResync {
            request_id: 5,
            requester_host: HostName::new("laptop"),
            target_host: HostName::new("desktop"),
            remaining_hops: 4,
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            since_seq: 12,
        })));

        test_helpers::assert_json_roundtrip(&msg);
    }

    #[test]
    fn message_peer_routed_resync_snapshot_roundtrip() {
        let msg = Message::Peer(Box::new(PeerWireMessage::Routed(RoutedPeerMessage::ResyncSnapshot {
            request_id: 6,
            requester_host: HostName::new("laptop"),
            responder_host: HostName::new("desktop"),
            remaining_hops: 4,
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            repo_path: PathBuf::from("/tmp/repo"),
            clock: VectorClock::default(),
            seq: 13,
            data: Box::new(ProviderData::default()),
        })));

        test_helpers::assert_json_roundtrip(&msg);
    }

    #[test]
    fn message_peer_goodbye_roundtrip() {
        let msg = Message::Peer(Box::new(PeerWireMessage::Goodbye { reason: GoodbyeReason::Superseded }));

        test_helpers::assert_json_roundtrip(&msg);
    }
}
