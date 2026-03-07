pub mod commands;
pub mod provider_data;
pub mod snapshot;

#[cfg(test)]
pub(crate) mod test_helpers {
    use serde::de::DeserializeOwned;
    use serde::Serialize;

    /// Assert JSON roundtrip via re-serialization (for types without PartialEq).
    pub fn assert_json_roundtrip<T: Serialize + DeserializeOwned + std::fmt::Debug>(value: &T) {
        let json = serde_json::to_string(value).expect("serialize");
        let decoded: T = serde_json::from_str(&json).expect("deserialize");
        let json2 = serde_json::to_string(&decoded).expect("re-serialize");
        assert_eq!(json2, json, "JSON roundtrip mismatch");
    }

    /// Assert JSON roundtrip via PartialEq (for types that derive it).
    pub fn assert_roundtrip<T: Serialize + DeserializeOwned + std::fmt::Debug + PartialEq>(
        value: &T,
    ) {
        let json = serde_json::to_string(value).expect("serialize");
        let decoded: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, *value);
    }
}

use serde::{Deserialize, Serialize};

pub use commands::{CheckoutStatus, Command, CommandResult};
pub use provider_data::{
    AheadBehind, AssociationKey, ChangeRequest, ChangeRequestStatus, Checkout, CloudAgentSession,
    CommitInfo, CorrelationKey, Issue, ProviderData, SessionStatus, WorkingTreeStatus, Workspace,
};
pub use snapshot::{
    CategoryLabels, CheckoutRef, ProviderError, RepoInfo, RepoLabels, Snapshot, WorkItem,
    WorkItemIdentity, WorkItemKind,
};

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
        Message::Response {
            id,
            ok: true,
            data: Some(serde_json::to_value(data).expect("response data must be serializable")),
            error: None,
        }
    }

    /// Build a success response with no payload.
    pub fn empty_ok_response(id: u64) -> Self {
        Message::Response {
            id,
            ok: true,
            data: None,
            error: None,
        }
    }

    /// Build an error response.
    pub fn error_response(id: u64, message: impl Into<String>) -> Self {
        Message::Response {
            id,
            ok: false,
            data: None,
            error: Some(message.into()),
        }
    }
}

/// Events pushed from daemon to subscribed clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum DaemonEvent {
    #[serde(rename = "snapshot")]
    Snapshot(Box<Snapshot>),
    #[serde(rename = "repo_added")]
    RepoAdded(Box<RepoInfo>),
    #[serde(rename = "repo_removed")]
    RepoRemoved { path: std::path::PathBuf },
    /// Async command completion notification for socket subscribers (Step 2).
    /// Not emitted in the in-process path where results are returned directly.
    #[serde(rename = "command_result")]
    CommandResult {
        repo: std::path::PathBuf,
        result: commands::CommandResult,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[test]
    fn message_request_roundtrip() {
        let msg = Message::Request {
            id: 42,
            method: "subscribe".to_string(),
            params: serde_json::json!({"repo": "/tmp/my-repo"}),
        };
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
        let msg = Message::Response {
            id: 1,
            ok: true,
            data: Some(serde_json::json!({"count": 42})),
            error: None,
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
        match deserialized {
            Message::Response {
                id,
                ok,
                data,
                error,
            } => {
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
        let msg = Message::Response {
            id: 2,
            ok: false,
            data: None,
            error: Some("not found".to_string()),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
        match deserialized {
            Message::Response {
                id,
                ok,
                data,
                error,
            } => {
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
            work_items: vec![WorkItem {
                kind: WorkItemKind::Checkout,
                identity: WorkItemIdentity::Checkout(PathBuf::from("/tmp/my-repo/wt")),
                branch: Some("feature-x".to_string()),
                description: "Feature X".to_string(),
                checkout: Some(CheckoutRef {
                    key: PathBuf::from("/tmp/my-repo/wt"),
                    is_main_checkout: false,
                }),
                change_request_key: Some("PR#10".to_string()),
                session_key: None,
                issue_keys: vec!["ISSUE-1".to_string()],
                workspace_refs: vec![],
                is_main_checkout: false,
                debug_group: vec![],
            }],
            providers: ProviderData::default(),
            provider_health: HashMap::from([
                ("git".to_string(), true),
                ("github".to_string(), false),
            ]),
            errors: vec![ProviderError {
                category: "github".to_string(),
                message: "rate limited".to_string(),
            }],
        };
        let msg = Message::Event {
            event: Box::new(DaemonEvent::Snapshot(Box::new(snapshot))),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
        match deserialized {
            Message::Event { event } => match *event {
                DaemonEvent::Snapshot(snap) => {
                    let snap = *snap;
                    assert_eq!(snap.seq, 7);
                    assert_eq!(snap.repo, PathBuf::from("/tmp/my-repo"));
                    assert_eq!(snap.work_items.len(), 1);
                    assert_eq!(snap.work_items[0].branch.as_deref(), Some("feature-x"));
                    assert_eq!(snap.work_items[0].kind, WorkItemKind::Checkout);
                    assert!(snap.provider_health["git"]);
                    assert!(!snap.provider_health["github"]);
                    assert_eq!(snap.errors.len(), 1);
                    assert_eq!(snap.errors[0].category, "github");
                }
                other => panic!("expected Snapshot event, got {:?}", other),
            },
            other => panic!("expected Event, got {:?}", other),
        }
    }

    #[test]
    fn ok_response_builds_with_serialized_data() {
        let data = serde_json::json!({"count": 42, "name": "test"});
        let msg = Message::ok_response(7, &data);
        match msg {
            Message::Response {
                id,
                ok,
                data,
                error,
            } => {
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
            Message::Response {
                id,
                ok,
                data,
                error,
            } => {
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
            Message::Response {
                id,
                ok,
                data,
                error,
            } => {
                assert_eq!(id, 5);
                assert!(!ok);
                assert!(data.is_none());
                assert_eq!(error.as_deref(), Some("something went wrong"));
            }
            other => panic!("expected Response, got {:?}", other),
        }
    }
}
