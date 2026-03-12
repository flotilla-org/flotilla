use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::delta::Change;
use crate::provider_data::ProviderData;
use crate::{HostName, RepoIdentity};

/// Message exchanged between peer daemons for multi-host data replication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerDataMessage {
    pub origin_host: HostName,
    pub repo_identity: RepoIdentity,
    pub repo_path: PathBuf,
    pub kind: PeerDataKind,
}

/// The payload kind within a peer data exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PeerDataKind {
    /// Full snapshot of provider data from the origin host.
    #[serde(rename = "snapshot")]
    Snapshot { data: Box<ProviderData>, seq: u64 },
    /// Incremental delta — only provider-data variants (Checkout, Branch, Workspace, Session, Issue).
    #[serde(rename = "delta")]
    Delta {
        changes: Vec<Change>,
        seq: u64,
        prev_seq: u64,
    },
    /// Request the peer to resend data from the given sequence number.
    #[serde(rename = "request_resync")]
    RequestResync { since_seq: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_data_message_snapshot_roundtrip() {
        let msg = PeerDataMessage {
            origin_host: HostName::new("desktop"),
            repo_identity: RepoIdentity {
                authority: "github.com".into(),
                path: "owner/repo".into(),
            },
            repo_path: PathBuf::from("/home/dev/repo"),
            kind: PeerDataKind::Snapshot {
                data: Box::new(ProviderData::default()),
                seq: 1,
            },
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: PeerDataMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.origin_host, msg.origin_host);
        assert_eq!(back.repo_identity, msg.repo_identity);
    }

    #[test]
    fn peer_data_message_request_resync() {
        let msg = PeerDataMessage {
            origin_host: HostName::new("cloud"),
            repo_identity: RepoIdentity {
                authority: "github.com".into(),
                path: "owner/repo".into(),
            },
            repo_path: PathBuf::from("/opt/repo"),
            kind: PeerDataKind::RequestResync { since_seq: 5 },
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: PeerDataMessage = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            back.kind,
            PeerDataKind::RequestResync { since_seq: 5 }
        ));
    }

    #[test]
    fn peer_data_message_delta_roundtrip() {
        use crate::delta::{Branch, BranchStatus, EntryOp};

        let msg = PeerDataMessage {
            origin_host: HostName::new("laptop"),
            repo_identity: RepoIdentity {
                authority: "github.com".into(),
                path: "owner/repo".into(),
            },
            repo_path: PathBuf::from("/home/dev/repo"),
            kind: PeerDataKind::Delta {
                changes: vec![Change::Branch {
                    key: "feat-x".into(),
                    op: EntryOp::Added(Branch {
                        status: BranchStatus::Remote,
                    }),
                }],
                seq: 3,
                prev_seq: 2,
            },
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: PeerDataMessage = serde_json::from_str(&json).expect("deserialize");
        match back.kind {
            PeerDataKind::Delta {
                changes,
                seq,
                prev_seq,
            } => {
                assert_eq!(seq, 3);
                assert_eq!(prev_seq, 2);
                assert_eq!(changes.len(), 1);
            }
            other => panic!("expected Delta, got {:?}", other),
        }
    }
}
