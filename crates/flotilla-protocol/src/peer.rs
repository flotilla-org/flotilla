use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{delta::Change, provider_data::ProviderData, CommandValue, HostSummary, NodeId, RepoIdentity, Step, StepOutcome, StepStatus};

/// Logical clock for causal ordering and deduplication of peer messages.
///
/// Each entry maps a node identity to a monotonically increasing counter.
/// A node increments its own entry when originating a message. Relay
/// nodes increment their own entry before forwarding. A message whose
/// clock is dominated by (≤ in every entry) the receiver's last-seen
/// clock is a duplicate and can be dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorClock(pub BTreeMap<NodeId, u64>);

impl VectorClock {
    /// Increment the entry for `node_id`, returning the new value.
    pub fn tick(&mut self, node_id: &NodeId) -> u64 {
        let entry = self.0.entry(node_id.clone()).or_insert(0);
        *entry += 1;
        *entry
    }

    /// Get the counter for a specific node, defaulting to 0.
    pub fn get(&self, node_id: &NodeId) -> u64 {
        self.0.get(node_id).copied().unwrap_or(0)
    }

    /// Returns true if `self` is dominated by `other` — every entry in
    /// `self` is ≤ the corresponding entry in `other`. This means the
    /// message represented by `self` has already been seen.
    pub fn dominated_by(&self, other: &VectorClock) -> bool {
        // All keys in self must be ≤ their counterpart in other
        for (node_id, &val) in &self.0 {
            if val > other.get(node_id) {
                return false;
            }
        }
        true
    }

    /// Merge `other` into `self`, taking the max of each entry.
    pub fn merge(&mut self, other: &VectorClock) {
        for (node_id, &val) in &other.0 {
            let entry = self.0.entry(node_id.clone()).or_insert(0);
            *entry = (*entry).max(val);
        }
    }
}

/// Message exchanged between peer daemons for multi-host data replication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerDataMessage {
    pub origin_node_id: NodeId,
    pub repo_identity: RepoIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_repo_root: Option<PathBuf>,
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
    HostSummary(HostSummary),
    Routed(RoutedPeerMessage),
    Goodbye { reason: GoodbyeReason },
    Ping { timestamp: u64 },
    Pong { timestamp: u64 },
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
        requester_node_id: NodeId,
        target_node_id: NodeId,
        remaining_hops: u8,
        repo_identity: RepoIdentity,
        since_seq: u64,
    },
    ResyncSnapshot {
        request_id: u64,
        requester_node_id: NodeId,
        responder_node_id: NodeId,
        remaining_hops: u8,
        repo_identity: RepoIdentity,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host_repo_root: Option<PathBuf>,
        clock: VectorClock,
        seq: u64,
        data: Box<ProviderData>,
    },
    CommandRequest {
        request_id: u64,
        requester_node_id: NodeId,
        target_node_id: NodeId,
        remaining_hops: u8,
        command: Box<crate::Command>,
        /// Session ID of the originating client, for cursor ownership on the target.
        #[serde(default)]
        session_id: Option<uuid::Uuid>,
    },
    CommandCancelRequest {
        cancel_id: u64,
        requester_node_id: NodeId,
        target_node_id: NodeId,
        remaining_hops: u8,
        command_request_id: u64,
    },
    CommandEvent {
        request_id: u64,
        requester_node_id: NodeId,
        responder_node_id: NodeId,
        remaining_hops: u8,
        event: Box<CommandPeerEvent>,
    },
    CommandResponse {
        request_id: u64,
        requester_node_id: NodeId,
        responder_node_id: NodeId,
        remaining_hops: u8,
        result: Box<crate::CommandValue>,
    },
    CommandCancelResponse {
        cancel_id: u64,
        requester_node_id: NodeId,
        responder_node_id: NodeId,
        remaining_hops: u8,
        error: Option<String>,
    },
    RemoteStepRequest {
        request_id: u64,
        requester_node_id: NodeId,
        target_node_id: NodeId,
        remaining_hops: u8,
        repo_identity: RepoIdentity,
        /// Global step index of the first step in this batch on the requester.
        ///
        /// The responder emits batch-relative progress indices only. The
        /// requester uses this offset when remapping them back onto the global
        /// command step sequence.
        step_offset: usize,
        /// Every step in the batch is expected to target `target_node_id`.
        /// The receiver should reject mismatched steps instead of trying to route them.
        steps: Vec<Step>,
    },
    RemoteStepEvent {
        request_id: u64,
        requester_node_id: NodeId,
        responder_node_id: NodeId,
        remaining_hops: u8,
        batch_step_index: usize,
        batch_step_count: usize,
        description: String,
        status: StepStatus,
    },
    RemoteStepResponse {
        request_id: u64,
        requester_node_id: NodeId,
        responder_node_id: NodeId,
        remaining_hops: u8,
        outcomes: Vec<StepOutcome>,
    },
    RemoteStepCancelRequest {
        cancel_id: u64,
        requester_node_id: NodeId,
        target_node_id: NodeId,
        remaining_hops: u8,
        remote_step_request_id: u64,
    },
    RemoteStepCancelResponse {
        cancel_id: u64,
        requester_node_id: NodeId,
        responder_node_id: NodeId,
        remaining_hops: u8,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum CommandPeerEvent {
    Started {
        repo_identity: RepoIdentity,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<PathBuf>,
        description: String,
    },
    StepUpdate {
        repo_identity: RepoIdentity,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<PathBuf>,
        step_index: usize,
        step_count: usize,
        description: String,
        status: StepStatus,
    },
    Finished {
        repo_identity: RepoIdentity,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<PathBuf>,
        result: CommandValue,
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
    Delta { changes: Vec<Change>, seq: u64, prev_seq: u64 },
    /// Request the peer to resend data from the given sequence number.
    #[serde(rename = "request_resync")]
    RequestResync { since_seq: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Command, CommandAction, CommandValue, HostEnvironment, HostProviderStatus, HostSummary, NodeId, RepoSelector, StepAction,
        StepExecutionContext, SystemInfo, ToolInventory,
    };

    fn sample_host_summary() -> HostSummary {
        HostSummary {
            node: crate::NodeInfo::new(NodeId::new("desktop"), "Desktop"),
            system: SystemInfo {
                home_dir: Some(PathBuf::from("/home/dev")),
                os: Some("linux".into()),
                arch: Some("aarch64".into()),
                cpu_count: Some(8),
                memory_total_mb: None,
                environment: HostEnvironment::Container,
            },
            inventory: ToolInventory::default(),
            providers: vec![HostProviderStatus { category: "vcs".into(), name: "Git".into(), implementation: "git".into(), healthy: true }],
            environments: vec![],
        }
    }

    #[test]
    fn vector_clock_tick_increments() {
        let mut clock = VectorClock::default();
        assert_eq!(clock.tick(&NodeId::new("a")), 1);
        assert_eq!(clock.tick(&NodeId::new("a")), 2);
        assert_eq!(clock.tick(&NodeId::new("b")), 1);
        assert_eq!(clock.get(&NodeId::new("a")), 2);
        assert_eq!(clock.get(&NodeId::new("b")), 1);
        assert_eq!(clock.get(&NodeId::new("c")), 0);
    }

    #[test]
    fn vector_clock_dominated_by() {
        let mut a = VectorClock::default();
        a.tick(&NodeId::new("x")); // {x: 1}

        let mut b = VectorClock::default();
        b.tick(&NodeId::new("x")); // {x: 1}
        b.tick(&NodeId::new("x")); // {x: 2}

        assert!(a.dominated_by(&b), "a{{x:1}} should be dominated by b{{x:2}}");
        assert!(!b.dominated_by(&a), "b{{x:2}} should not be dominated by a{{x:1}}");

        // Equal clocks dominate each other
        let c = a.clone();
        assert!(a.dominated_by(&c));
        assert!(c.dominated_by(&a));
    }

    #[test]
    fn vector_clock_dominated_by_with_different_keys() {
        let mut a = VectorClock::default();
        a.tick(&NodeId::new("x")); // {x: 1}

        let mut b = VectorClock::default();
        b.tick(&NodeId::new("y")); // {y: 1}

        // a has x:1 but b has x:0 → not dominated
        assert!(!a.dominated_by(&b));
        // b has y:1 but a has y:0 → not dominated
        assert!(!b.dominated_by(&a));
    }

    #[test]
    fn vector_clock_merge_takes_max() {
        let mut a = VectorClock::default();
        a.tick(&NodeId::new("x")); // {x: 1}
        a.tick(&NodeId::new("x")); // {x: 2}

        let mut b = VectorClock::default();
        b.tick(&NodeId::new("x")); // {x: 1}
        b.tick(&NodeId::new("y")); // {y: 1}

        a.merge(&b);
        assert_eq!(a.get(&NodeId::new("x")), 2); // max(2, 1)
        assert_eq!(a.get(&NodeId::new("y")), 1); // max(0, 1)
    }

    #[test]
    fn vector_clock_serde_default() {
        // Deserializing a message without a clock field should give default
        let json = r#"{"origin_node_id":"desktop","repo_identity":{"authority":"github.com","path":"owner/repo"},"kind":{"type":"request_resync","since_seq":0}}"#;
        let msg: PeerDataMessage = serde_json::from_str(json).expect("deserialize without clock");
        assert!(msg.clock.0.is_empty(), "default clock should be empty");
        assert_eq!(msg.host_repo_root, None);
    }

    #[test]
    fn peer_data_message_snapshot_roundtrip() {
        let msg = PeerDataMessage {
            origin_node_id: NodeId::new("desktop"),
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            host_repo_root: Some(PathBuf::from("/home/dev/repo")),
            clock: VectorClock::default(),
            kind: PeerDataKind::Snapshot { data: Box::new(ProviderData::default()), seq: 1 },
        };
        let json_value = serde_json::to_value(&msg).expect("serialize");
        assert_eq!(json_value["origin_node_id"], "desktop");
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: PeerDataMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.origin_node_id, msg.origin_node_id);
        assert_eq!(back.repo_identity, msg.repo_identity);
        assert_eq!(back.host_repo_root, msg.host_repo_root);
    }

    #[test]
    fn peer_data_message_snapshot_roundtrip_without_host_repo_root() {
        let msg = PeerDataMessage {
            origin_node_id: NodeId::new("desktop"),
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            host_repo_root: None,
            clock: VectorClock::default(),
            kind: PeerDataKind::Snapshot { data: Box::new(ProviderData::default()), seq: 1 },
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(!json.contains("\"host_repo_root\""), "optional host repo root should be omitted when absent: {json}");
        let back: PeerDataMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.host_repo_root, None);
    }

    #[test]
    fn peer_data_message_request_resync() {
        let msg = PeerDataMessage {
            origin_node_id: NodeId::new("cloud"),
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            host_repo_root: Some(PathBuf::from("/opt/repo")),
            clock: VectorClock::default(),
            kind: PeerDataKind::RequestResync { since_seq: 5 },
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: PeerDataMessage = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(back.kind, PeerDataKind::RequestResync { since_seq: 5 }));
        assert_eq!(back.host_repo_root, Some(PathBuf::from("/opt/repo")));
    }

    #[test]
    fn ping_pong_roundtrip() {
        let ping = PeerWireMessage::Ping { timestamp: 1234567890 };
        let json = serde_json::to_string(&ping).expect("serialize ping");
        let decoded: PeerWireMessage = serde_json::from_str(&json).expect("deserialize ping");
        match decoded {
            PeerWireMessage::Ping { timestamp } => assert_eq!(timestamp, 1234567890),
            other => panic!("expected Ping, got {:?}", other),
        }

        let pong = PeerWireMessage::Pong { timestamp: 1234567890 };
        let json = serde_json::to_string(&pong).expect("serialize pong");
        let decoded: PeerWireMessage = serde_json::from_str(&json).expect("deserialize pong");
        match decoded {
            PeerWireMessage::Pong { timestamp } => assert_eq!(timestamp, 1234567890),
            other => panic!("expected Pong, got {:?}", other),
        }
    }

    #[test]
    fn peer_wire_message_host_summary_roundtrip() {
        let msg = PeerWireMessage::HostSummary(sample_host_summary());
        let json = serde_json::to_string(&msg).expect("serialize");
        let decoded: PeerWireMessage = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(decoded, PeerWireMessage::HostSummary(_)));
    }

    #[test]
    fn peer_data_message_delta_roundtrip() {
        use crate::delta::{Branch, BranchStatus, EntryOp};

        let msg = PeerDataMessage {
            origin_node_id: NodeId::new("laptop"),
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            host_repo_root: Some(PathBuf::from("/home/dev/repo")),
            clock: VectorClock::default(),
            kind: PeerDataKind::Delta {
                changes: vec![Change::Branch { key: "feat-x".into(), op: EntryOp::Added(Branch { status: BranchStatus::Remote }) }],
                seq: 3,
                prev_seq: 2,
            },
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: PeerDataMessage = serde_json::from_str(&json).expect("deserialize");
        match back.kind {
            PeerDataKind::Delta { changes, seq, prev_seq } => {
                assert_eq!(seq, 3);
                assert_eq!(prev_seq, 2);
                assert_eq!(changes.len(), 1);
            }
            other => panic!("expected Delta, got {:?}", other),
        }
    }

    #[test]
    fn routed_command_request_roundtrip() {
        let msg = RoutedPeerMessage::CommandRequest {
            request_id: 42,
            requester_node_id: NodeId::new("workstation"),
            target_node_id: NodeId::new("feta"),
            remaining_hops: 7,
            command: Box::new(Command {
                node_id: Some(NodeId::new("feta")),
                provisioning_target: None,
                context_repo: None,
                action: CommandAction::Refresh { repo: Some(RepoSelector::Query("flotilla".into())) },
            }),
            session_id: None,
        };
        let json_value = serde_json::to_value(&msg).expect("serialize");
        assert_eq!(json_value["requester_node_id"], "workstation");
        assert_eq!(json_value["target_node_id"], "feta");
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: RoutedPeerMessage = serde_json::from_str(&json).expect("deserialize");
        match back {
            RoutedPeerMessage::CommandRequest { request_id, requester_node_id, target_node_id, remaining_hops, .. } => {
                assert_eq!(request_id, 42);
                assert_eq!(requester_node_id, NodeId::new("workstation"));
                assert_eq!(target_node_id, NodeId::new("feta"));
                assert_eq!(remaining_hops, 7);
            }
            other => panic!("expected CommandRequest, got {:?}", other),
        }
    }

    #[test]
    fn routed_command_response_roundtrip() {
        let msg = RoutedPeerMessage::CommandResponse {
            request_id: 42,
            requester_node_id: NodeId::new("workstation"),
            responder_node_id: NodeId::new("feta"),
            remaining_hops: 7,
            result: Box::new(CommandValue::RepoTracked { path: PathBuf::from("/srv/repo"), resolved_from: None }),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: RoutedPeerMessage = serde_json::from_str(&json).expect("deserialize");
        match back {
            RoutedPeerMessage::CommandResponse { request_id, requester_node_id, responder_node_id, remaining_hops, result } => {
                assert_eq!(request_id, 42);
                assert_eq!(requester_node_id, NodeId::new("workstation"));
                assert_eq!(responder_node_id, NodeId::new("feta"));
                assert_eq!(remaining_hops, 7);
                assert!(matches!(*result, CommandValue::RepoTracked { .. }));
            }
            other => panic!("expected CommandResponse, got {:?}", other),
        }
    }

    #[test]
    fn routed_command_cancel_request_roundtrip() {
        let msg = RoutedPeerMessage::CommandCancelRequest {
            cancel_id: 7,
            requester_node_id: NodeId::new("workstation"),
            target_node_id: NodeId::new("feta"),
            remaining_hops: 4,
            command_request_id: 42,
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: RoutedPeerMessage = serde_json::from_str(&json).expect("deserialize");
        match back {
            RoutedPeerMessage::CommandCancelRequest {
                cancel_id,
                requester_node_id,
                target_node_id,
                remaining_hops,
                command_request_id,
            } => {
                assert_eq!(cancel_id, 7);
                assert_eq!(requester_node_id, NodeId::new("workstation"));
                assert_eq!(target_node_id, NodeId::new("feta"));
                assert_eq!(remaining_hops, 4);
                assert_eq!(command_request_id, 42);
            }
            other => panic!("expected CommandCancelRequest, got {:?}", other),
        }
    }

    #[test]
    fn routed_command_event_roundtrip() {
        let msg = RoutedPeerMessage::CommandEvent {
            request_id: 9,
            requester_node_id: NodeId::new("workstation"),
            responder_node_id: NodeId::new("feta"),
            remaining_hops: 5,
            event: Box::new(CommandPeerEvent::StepUpdate {
                repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
                repo: Some(PathBuf::from("/repo")),
                step_index: 1,
                step_count: 3,
                description: "Creating worktree".into(),
                status: StepStatus::Started,
            }),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: RoutedPeerMessage = serde_json::from_str(&json).expect("deserialize");
        match back {
            RoutedPeerMessage::CommandEvent { request_id, requester_node_id, responder_node_id, remaining_hops, event } => {
                assert_eq!(request_id, 9);
                assert_eq!(requester_node_id, NodeId::new("workstation"));
                assert_eq!(responder_node_id, NodeId::new("feta"));
                assert_eq!(remaining_hops, 5);
                assert_eq!(*event, CommandPeerEvent::StepUpdate {
                    repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
                    repo: Some(PathBuf::from("/repo")),
                    step_index: 1,
                    step_count: 3,
                    description: "Creating worktree".into(),
                    status: StepStatus::Started,
                });
            }
            other => panic!("expected CommandEvent, got {:?}", other),
        }
    }

    #[test]
    fn routed_command_cancel_response_roundtrip() {
        let msg = RoutedPeerMessage::CommandCancelResponse {
            cancel_id: 7,
            requester_node_id: NodeId::new("workstation"),
            responder_node_id: NodeId::new("feta"),
            remaining_hops: 4,
            error: Some("command not found".into()),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: RoutedPeerMessage = serde_json::from_str(&json).expect("deserialize");
        match back {
            RoutedPeerMessage::CommandCancelResponse { cancel_id, requester_node_id, responder_node_id, remaining_hops, error } => {
                assert_eq!(cancel_id, 7);
                assert_eq!(requester_node_id, NodeId::new("workstation"));
                assert_eq!(responder_node_id, NodeId::new("feta"));
                assert_eq!(remaining_hops, 4);
                assert_eq!(error.as_deref(), Some("command not found"));
            }
            other => panic!("expected CommandCancelResponse, got {:?}", other),
        }
    }

    #[test]
    fn routed_remote_step_request_roundtrip() {
        let msg = RoutedPeerMessage::RemoteStepRequest {
            request_id: 43,
            requester_node_id: NodeId::new("workstation"),
            target_node_id: NodeId::new("feta"),
            remaining_hops: 6,
            repo_identity: RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() },
            step_offset: 2,
            steps: vec![Step {
                description: "Prepare terminal".into(),
                host: StepExecutionContext::Host(NodeId::new("feta")),
                action: StepAction::PrepareTerminalForCheckout {
                    checkout_path: crate::ExecutionEnvironmentPath::new("/repo"),
                    commands: vec![],
                },
            }],
        };
        let json_value = serde_json::to_value(&msg).expect("serialize");
        assert_eq!(json_value["requester_node_id"], "workstation");
        assert_eq!(json_value["target_node_id"], "feta");
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(!json.contains("\"repo_path\""), "remote step request should not serialize repo root path: {json}");
        let back: RoutedPeerMessage = serde_json::from_str(&json).expect("deserialize");
        match back {
            RoutedPeerMessage::RemoteStepRequest {
                request_id,
                requester_node_id,
                target_node_id,
                remaining_hops,
                repo_identity,
                step_offset,
                steps,
            } => {
                assert_eq!(request_id, 43);
                assert_eq!(requester_node_id, NodeId::new("workstation"));
                assert_eq!(target_node_id, NodeId::new("feta"));
                assert_eq!(remaining_hops, 6);
                assert_eq!(repo_identity, RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() });
                assert_eq!(step_offset, 2);
                assert_eq!(steps.len(), 1);
            }
            other => panic!("expected RemoteStepRequest, got {:?}", other),
        }
    }

    #[test]
    fn routed_remote_step_event_roundtrip() {
        let msg = RoutedPeerMessage::RemoteStepEvent {
            request_id: 43,
            requester_node_id: NodeId::new("workstation"),
            responder_node_id: NodeId::new("feta"),
            remaining_hops: 6,
            batch_step_index: 0,
            batch_step_count: 1,
            description: "Prepare terminal".into(),
            status: StepStatus::Started,
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: RoutedPeerMessage = serde_json::from_str(&json).expect("deserialize");
        match back {
            RoutedPeerMessage::RemoteStepEvent {
                request_id,
                requester_node_id,
                responder_node_id,
                remaining_hops,
                batch_step_index,
                batch_step_count,
                description,
                status,
            } => {
                assert_eq!(request_id, 43);
                assert_eq!(requester_node_id, NodeId::new("workstation"));
                assert_eq!(responder_node_id, NodeId::new("feta"));
                assert_eq!(remaining_hops, 6);
                assert_eq!(batch_step_index, 0);
                assert_eq!(batch_step_count, 1);
                assert_eq!(description, "Prepare terminal");
                assert_eq!(status, StepStatus::Started);
            }
            other => panic!("expected RemoteStepEvent, got {:?}", other),
        }
    }

    #[test]
    fn routed_remote_step_response_roundtrip() {
        let msg = RoutedPeerMessage::RemoteStepResponse {
            request_id: 43,
            requester_node_id: NodeId::new("workstation"),
            responder_node_id: NodeId::new("feta"),
            remaining_hops: 6,
            outcomes: vec![StepOutcome::Completed, StepOutcome::Produced(CommandValue::Ok)],
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: RoutedPeerMessage = serde_json::from_str(&json).expect("deserialize");
        match back {
            RoutedPeerMessage::RemoteStepResponse { request_id, requester_node_id, responder_node_id, remaining_hops, outcomes } => {
                assert_eq!(request_id, 43);
                assert_eq!(requester_node_id, NodeId::new("workstation"));
                assert_eq!(responder_node_id, NodeId::new("feta"));
                assert_eq!(remaining_hops, 6);
                assert_eq!(outcomes, vec![StepOutcome::Completed, StepOutcome::Produced(CommandValue::Ok)]);
            }
            other => panic!("expected RemoteStepResponse, got {:?}", other),
        }
    }

    #[test]
    fn routed_remote_step_cancel_request_roundtrip() {
        let msg = RoutedPeerMessage::RemoteStepCancelRequest {
            cancel_id: 7,
            requester_node_id: NodeId::new("workstation"),
            target_node_id: NodeId::new("feta"),
            remaining_hops: 6,
            remote_step_request_id: 43,
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: RoutedPeerMessage = serde_json::from_str(&json).expect("deserialize");
        match back {
            RoutedPeerMessage::RemoteStepCancelRequest {
                cancel_id,
                requester_node_id,
                target_node_id,
                remaining_hops,
                remote_step_request_id,
            } => {
                assert_eq!(cancel_id, 7);
                assert_eq!(requester_node_id, NodeId::new("workstation"));
                assert_eq!(target_node_id, NodeId::new("feta"));
                assert_eq!(remaining_hops, 6);
                assert_eq!(remote_step_request_id, 43);
            }
            other => panic!("expected RemoteStepCancelRequest, got {:?}", other),
        }
    }

    #[test]
    fn routed_remote_step_cancel_response_roundtrip() {
        let msg = RoutedPeerMessage::RemoteStepCancelResponse {
            cancel_id: 7,
            requester_node_id: NodeId::new("workstation"),
            responder_node_id: NodeId::new("feta"),
            remaining_hops: 6,
            error: Some("remote step not found".into()),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: RoutedPeerMessage = serde_json::from_str(&json).expect("deserialize");
        match back {
            RoutedPeerMessage::RemoteStepCancelResponse { cancel_id, requester_node_id, responder_node_id, remaining_hops, error } => {
                assert_eq!(cancel_id, 7);
                assert_eq!(requester_node_id, NodeId::new("workstation"));
                assert_eq!(responder_node_id, NodeId::new("feta"));
                assert_eq!(remaining_hops, 6);
                assert_eq!(error.as_deref(), Some("remote step not found"));
            }
            other => panic!("expected RemoteStepCancelResponse, got {:?}", other),
        }
    }
}
