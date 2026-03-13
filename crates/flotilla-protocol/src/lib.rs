pub mod commands;
pub mod delta;
mod host;
pub mod peer;
pub mod provider_data;
pub mod snapshot;

pub use host::{HostName, HostPath, RepoIdentity};
pub use peer::{GoodbyeReason, PeerDataKind, PeerDataMessage, PeerWireMessage, RoutedPeerMessage, VectorClock};

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

pub use commands::{CheckoutStatus, Command, CommandResult, StepStatus};
pub use delta::{Branch, BranchStatus, Change, DeltaEntry, EntryOp};
pub use provider_data::{
    AheadBehind, AssociationKey, ChangeRequest, ChangeRequestStatus, Checkout, CloudAgentSession, CommitInfo, CorrelationKey, Issue,
    IssueChangeset, IssuePage, ManagedTerminal, ManagedTerminalId, ProviderData, SessionStatus, TerminalStatus, WorkingTreeStatus,
    Workspace,
};
use serde::{Deserialize, Serialize};
pub use snapshot::{CategoryLabels, CheckoutRef, ProviderError, RepoInfo, RepoLabels, Snapshot, WorkItem, WorkItemIdentity, WorkItemKind};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConfigLabel(pub String);

pub const PROTOCOL_VERSION: u32 = 1;

/// Top-level message envelope for the JSON protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Message {
    #[serde(rename = "request")]
    Request {
        id: u64,
        method: String,
        #[serde(default)]
        params: serde_json::Value,
    },
    #[serde(rename = "response")]
    Response {
        id: u64,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    #[serde(rename = "event")]
    Event { event: Box<DaemonEvent> },
    #[serde(rename = "hello")]
    Hello { protocol_version: u32, host_name: HostName },
    #[serde(rename = "peer")]
    Peer(Box<PeerWireMessage>),
}

/// Parsed response from the wire — before type-specific deserialization.
#[derive(Debug)]
pub struct RawResponse {
    pub ok: bool,
    pub data: Option<serde_json::Value>,
    pub error: Option<String>,
}

impl RawResponse {
    /// Parse the data payload into the expected type.
    pub fn parse<T: serde::de::DeserializeOwned>(self) -> Result<T, String> {
        if !self.ok {
            return Err(self.error.unwrap_or_else(|| "unknown error".into()));
        }
        let data = self.data.ok_or("response missing data field")?;
        serde_json::from_value(data).map_err(|e| format!("failed to parse response: {e}"))
    }

    /// Parse a response with no data payload (refresh, add_repo, remove_repo).
    pub fn parse_empty(self) -> Result<(), String> {
        if !self.ok {
            return Err(self.error.unwrap_or_else(|| "unknown error".into()));
        }
        Ok(())
    }
}

impl Message {
    /// Build a success response with a serializable payload.
    pub fn ok_response<T: serde::Serialize>(id: u64, data: &T) -> Self {
        Message::Response { id, ok: true, data: Some(serde_json::to_value(data).expect("response data must be serializable")), error: None }
    }

    /// Build a success response with no payload.
    pub fn empty_ok_response(id: u64) -> Self {
        Message::Response { id, ok: true, data: None, error: None }
    }

    /// Build an error response.
    pub fn error_response(id: u64, message: impl Into<String>) -> Self {
        Message::Response { id, ok: false, data: None, error: Some(message.into()) }
    }
}

/// Events pushed from daemon to subscribed clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum DaemonEvent {
    /// Full snapshot — sent on initial connect, after seq gaps, or when delta
    /// would be larger than the full snapshot.
    #[serde(rename = "snapshot_full")]
    SnapshotFull(Box<Snapshot>),
    /// Incremental delta — sent when only a subset of data changed.
    #[serde(rename = "snapshot_delta")]
    SnapshotDelta(Box<SnapshotDelta>),
    #[serde(rename = "repo_added")]
    RepoAdded(Box<RepoInfo>),
    #[serde(rename = "repo_removed")]
    RepoRemoved { path: std::path::PathBuf },
    #[serde(rename = "command_started")]
    CommandStarted { command_id: u64, repo: std::path::PathBuf, description: String },
    #[serde(rename = "command_finished")]
    CommandFinished { command_id: u64, repo: std::path::PathBuf, result: commands::CommandResult },
    #[serde(rename = "command_step_update")]
    CommandStepUpdate {
        command_id: u64,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerConnectionState {
    Connected,
    Disconnected,
    Connecting,
    Reconnecting,
}

/// A delta update for a repo snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotDelta {
    pub seq: u64,
    pub prev_seq: u64,
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

    #[test]
    fn message_request_roundtrip() {
        let msg = Message::Request { id: 42, method: "subscribe".to_string(), params: serde_json::json!({"repo": "/tmp/my-repo"}) };
        let json = serde_json::to_string(&msg).expect("serialize");
        let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
        match deserialized {
            Message::Request { id, method, params } => {
                assert_eq!(id, 42);
                assert_eq!(method, "subscribe");
                assert_eq!(params["repo"], "/tmp/my-repo");
            }
            other => panic!("expected Request, got {:?}", other),
        }
    }

    #[test]
    fn message_response_roundtrip() {
        // ok=true with data
        let msg = Message::Response { id: 1, ok: true, data: Some(serde_json::json!({"count": 42})), error: None };
        let json = serde_json::to_string(&msg).expect("serialize");
        let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
        match deserialized {
            Message::Response { id, ok, data, error } => {
                assert_eq!(id, 1);
                assert!(ok);
                assert_eq!(data.unwrap()["count"], 42);
                assert!(error.is_none());
            }
            other => panic!("expected Response, got {:?}", other),
        }
        // error=None should not appear in JSON
        assert!(!json.contains("error"));

        // ok=false with error
        let msg = Message::Response { id: 2, ok: false, data: None, error: Some("not found".to_string()) };
        let json = serde_json::to_string(&msg).expect("serialize");
        let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
        match deserialized {
            Message::Response { id, ok, data, error } => {
                assert_eq!(id, 2);
                assert!(!ok);
                assert!(data.is_none());
                assert_eq!(error.as_deref(), Some("not found"));
            }
            other => panic!("expected Response, got {:?}", other),
        }
        // data=None should not appear in JSON
        assert!(!json.contains("data"));
    }

    #[test]
    fn message_event_snapshot_roundtrip() {
        let snapshot = Snapshot {
            seq: 7,
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
                ("code_review".to_string(), HashMap::from([("GitHub".to_string(), false)])),
            ]),
            errors: vec![ProviderError { category: "github".to_string(), provider: String::new(), message: "rate limited".to_string() }],
            issue_total: None,
            issue_has_more: false,
            issue_search_results: None,
        };
        let msg = Message::Event { event: Box::new(DaemonEvent::SnapshotFull(Box::new(snapshot))) };
        let json = serde_json::to_string(&msg).expect("serialize");
        let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
        match deserialized {
            Message::Event { event } => match *event {
                DaemonEvent::SnapshotFull(snap) => {
                    let snap = *snap;
                    assert_eq!(snap.seq, 7);
                    assert_eq!(snap.repo, PathBuf::from("/tmp/my-repo"));
                    assert_eq!(snap.work_items.len(), 1);
                    assert_eq!(snap.work_items[0].branch.as_deref(), Some("feature-x"));
                    assert_eq!(snap.work_items[0].kind, WorkItemKind::Checkout);
                    assert!(snap.provider_health["vcs"]["Git"]);
                    assert!(!snap.provider_health["code_review"]["GitHub"]);
                    assert_eq!(snap.errors.len(), 1);
                    assert_eq!(snap.errors[0].category, "github");
                }
                other => panic!("expected Snapshot event, got {:?}", other),
            },
            other => panic!("expected Event, got {:?}", other),
        }
    }

    #[test]
    fn snapshot_delta_event_roundtrip() {
        let delta = SnapshotDelta {
            seq: 3,
            prev_seq: 2,
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
        let msg = Message::Event { event: Box::new(DaemonEvent::SnapshotDelta(Box::new(delta))) };
        let json = serde_json::to_string(&msg).expect("serialize");
        let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
        match deserialized {
            Message::Event { event } => match *event {
                DaemonEvent::SnapshotDelta(d) => {
                    assert_eq!(d.seq, 3);
                    assert_eq!(d.prev_seq, 2);
                    assert_eq!(d.repo, PathBuf::from("/tmp/my-repo"));
                    assert_eq!(d.changes.len(), 2);
                    assert_eq!(d.issue_total, Some(100));
                    assert!(d.issue_has_more);
                    assert!(d.issue_search_results.is_none());
                }
                other => panic!("expected SnapshotDelta, got {:?}", other),
            },
            other => panic!("expected Event, got {:?}", other),
        }
    }

    #[test]
    fn ok_response_builds_with_serialized_data() {
        let data = serde_json::json!({"count": 42, "name": "test"});
        let msg = Message::ok_response(7, &data);
        match msg {
            Message::Response { id, ok, data, error } => {
                assert_eq!(id, 7);
                assert!(ok);
                let d = data.expect("should have data");
                assert_eq!(d["count"], 42);
                assert_eq!(d["name"], "test");
                assert!(error.is_none());
            }
            other => panic!("expected Response, got {:?}", other),
        }
    }

    #[test]
    fn empty_ok_response_builds_with_no_data() {
        let msg = Message::empty_ok_response(99);
        match msg {
            Message::Response { id, ok, data, error } => {
                assert_eq!(id, 99);
                assert!(ok);
                assert!(data.is_none());
                assert!(error.is_none());
            }
            other => panic!("expected Response, got {:?}", other),
        }
    }

    #[test]
    fn error_response_builds_with_error_message() {
        let msg = Message::error_response(5, "something went wrong");
        match msg {
            Message::Response { id, ok, data, error } => {
                assert_eq!(id, 5);
                assert!(!ok);
                assert!(data.is_none());
                assert_eq!(error.as_deref(), Some("something went wrong"));
            }
            other => panic!("expected Response, got {:?}", other),
        }
    }

    #[test]
    fn daemon_event_command_started_roundtrip() {
        let event = DaemonEvent::CommandStarted {
            command_id: 42,
            repo: PathBuf::from("/tmp/repo"),
            description: "Creating checkout...".to_string(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let decoded: DaemonEvent = serde_json::from_str(&json).expect("deserialize");
        match decoded {
            DaemonEvent::CommandStarted { command_id, repo, description } => {
                assert_eq!(command_id, 42);
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
            repo: PathBuf::from("/tmp/repo"),
            result: CommandResult::CheckoutCreated { branch: "feat-x".into() },
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let decoded: DaemonEvent = serde_json::from_str(&json).expect("deserialize");
        match decoded {
            DaemonEvent::CommandFinished { command_id, repo, result } => {
                assert_eq!(command_id, 42);
                assert_eq!(repo, PathBuf::from("/tmp/repo"));
                match result {
                    CommandResult::CheckoutCreated { branch } => assert_eq!(branch, "feat-x"),
                    other => panic!("expected CheckoutCreated, got {:?}", other),
                }
            }
            other => panic!("expected CommandFinished, got {:?}", other),
        }
    }

    #[test]
    fn message_hello_roundtrip() {
        let msg = Message::Hello { protocol_version: 1, host_name: HostName::new("desktop") };

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
