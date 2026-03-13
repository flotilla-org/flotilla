use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::delta::Change;
use crate::provider_data::ProviderData;
use crate::{HostName, RepoIdentity};

/// Logical clock for causal ordering and deduplication of peer messages.
///
/// Each entry maps a host name to a monotonically increasing counter.
/// A host increments its own entry when originating a message. Relay
/// nodes increment their own entry before forwarding. A message whose
/// clock is dominated by (≤ in every entry) the receiver's last-seen
/// clock is a duplicate and can be dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorClock(pub BTreeMap<HostName, u64>);

impl VectorClock {
    /// Increment the entry for `host`, returning the new value.
    pub fn tick(&mut self, host: &HostName) -> u64 {
        let entry = self.0.entry(host.clone()).or_insert(0);
        *entry += 1;
        *entry
    }

    /// Get the counter for a specific host, defaulting to 0.
    pub fn get(&self, host: &HostName) -> u64 {
        self.0.get(host).copied().unwrap_or(0)
    }

    /// Returns true if `self` is dominated by `other` — every entry in
    /// `self` is ≤ the corresponding entry in `other`. This means the
    /// message represented by `self` has already been seen.
    pub fn dominated_by(&self, other: &VectorClock) -> bool {
        // All keys in self must be ≤ their counterpart in other
        for (host, &val) in &self.0 {
            if val > other.get(host) {
                return false;
            }
        }
        true
    }

    /// Merge `other` into `self`, taking the max of each entry.
    pub fn merge(&mut self, other: &VectorClock) {
        for (host, &val) in &other.0 {
            let entry = self.0.entry(host.clone()).or_insert(0);
            *entry = (*entry).max(val);
        }
    }
}

/// Message exchanged between peer daemons for multi-host data replication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerDataMessage {
    pub origin_host: HostName,
    pub repo_identity: RepoIdentity,
    pub repo_path: PathBuf,
    /// Vector clock for causal ordering and deduplication.
    #[serde(default)]
    pub clock: VectorClock,
    pub kind: PeerDataKind,
}

/// Unified peer-to-peer wire payload used inside `Message::Peer`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "peer_type")]
pub enum PeerWireMessage {
    Data(PeerDataMessage),
    Routed(RoutedPeerMessage),
    Goodbye { reason: GoodbyeReason },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GoodbyeReason {
    Superseded,
}

/// Targeted peer control messages that are routed hop-by-hop rather than flooded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "routed_type")]
pub enum RoutedPeerMessage {
    RequestResync {
        request_id: u64,
        requester_host: HostName,
        target_host: HostName,
        remaining_hops: u8,
        repo_identity: RepoIdentity,
        since_seq: u64,
    },
    ResyncSnapshot {
        request_id: u64,
        requester_host: HostName,
        responder_host: HostName,
        remaining_hops: u8,
        repo_identity: RepoIdentity,
        repo_path: PathBuf,
        clock: VectorClock,
        seq: u64,
        data: Box<ProviderData>,
    },
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
    fn vector_clock_tick_increments() {
        let mut clock = VectorClock::default();
        assert_eq!(clock.tick(&HostName::new("a")), 1);
        assert_eq!(clock.tick(&HostName::new("a")), 2);
        assert_eq!(clock.tick(&HostName::new("b")), 1);
        assert_eq!(clock.get(&HostName::new("a")), 2);
        assert_eq!(clock.get(&HostName::new("b")), 1);
        assert_eq!(clock.get(&HostName::new("c")), 0);
    }

    #[test]
    fn vector_clock_dominated_by() {
        let mut a = VectorClock::default();
        a.tick(&HostName::new("x")); // {x: 1}

        let mut b = VectorClock::default();
        b.tick(&HostName::new("x")); // {x: 1}
        b.tick(&HostName::new("x")); // {x: 2}

        assert!(
            a.dominated_by(&b),
            "a{{x:1}} should be dominated by b{{x:2}}"
        );
        assert!(
            !b.dominated_by(&a),
            "b{{x:2}} should not be dominated by a{{x:1}}"
        );

        // Equal clocks dominate each other
        let c = a.clone();
        assert!(a.dominated_by(&c));
        assert!(c.dominated_by(&a));
    }

    #[test]
    fn vector_clock_dominated_by_with_different_keys() {
        let mut a = VectorClock::default();
        a.tick(&HostName::new("x")); // {x: 1}

        let mut b = VectorClock::default();
        b.tick(&HostName::new("y")); // {y: 1}

        // a has x:1 but b has x:0 → not dominated
        assert!(!a.dominated_by(&b));
        // b has y:1 but a has y:0 → not dominated
        assert!(!b.dominated_by(&a));
    }

    #[test]
    fn vector_clock_merge_takes_max() {
        let mut a = VectorClock::default();
        a.tick(&HostName::new("x")); // {x: 1}
        a.tick(&HostName::new("x")); // {x: 2}

        let mut b = VectorClock::default();
        b.tick(&HostName::new("x")); // {x: 1}
        b.tick(&HostName::new("y")); // {y: 1}

        a.merge(&b);
        assert_eq!(a.get(&HostName::new("x")), 2); // max(2, 1)
        assert_eq!(a.get(&HostName::new("y")), 1); // max(0, 1)
    }

    #[test]
    fn vector_clock_serde_default() {
        // Deserializing a message without a clock field should give default
        let json = r#"{"origin_host":"desktop","repo_identity":{"authority":"github.com","path":"owner/repo"},"repo_path":"/repo","kind":{"type":"request_resync","since_seq":0}}"#;
        let msg: PeerDataMessage = serde_json::from_str(json).expect("deserialize without clock");
        assert!(msg.clock.0.is_empty(), "default clock should be empty");
    }

    #[test]
    fn peer_data_message_snapshot_roundtrip() {
        let msg = PeerDataMessage {
            origin_host: HostName::new("desktop"),
            repo_identity: RepoIdentity {
                authority: "github.com".into(),
                path: "owner/repo".into(),
            },
            repo_path: PathBuf::from("/home/dev/repo"),
            clock: VectorClock::default(),
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
            clock: VectorClock::default(),
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
            clock: VectorClock::default(),
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
