use std::{
    collections::HashMap,
    mem,
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use flotilla_protocol::{
    Command, CommandPeerEvent, CommandValue, ConfigLabel, EnvironmentId, GoodbyeReason, HostListEntry, HostListResponse, HostName,
    HostSummary, NodeId, NodeInfo, PeerConnectionState, PeerReconnectStatus, PeerWireMessage, RepoIdentity, RoutedPeerMessage, Step,
    StepOutcome, StepStatus, TopologyRoute,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::transport::{PeerSender, PeerTransport};

/// Result of handling an inbound peer transport message.
#[derive(Debug, PartialEq, Eq)]
pub enum HandleResult {
    /// Peer intentionally retired this connection; reconnect should be suppressed briefly.
    ReconnectSuppressed { peer: NodeId },
    /// A routed command targeted this daemon and should be executed locally.
    CommandRequested { request_id: u64, requester_node_id: NodeId, reply_via: NodeId, command: Command, session_id: Option<uuid::Uuid> },
    /// A routed command cancel request targeted this daemon.
    CommandCancelRequested { cancel_id: u64, requester_node_id: NodeId, reply_via: NodeId, command_request_id: u64 },
    /// A routed command lifecycle event reached the original requester.
    CommandEventReceived { request_id: u64, responder_node_id: NodeId, event: CommandPeerEvent },
    /// A routed command completed and the final result reached the requester.
    CommandResponseReceived { request_id: u64, responder_node_id: NodeId, result: CommandValue },
    /// A routed command cancel response reached the original requester.
    CommandCancelResponseReceived { cancel_id: u64, responder_node_id: NodeId, error: Option<String> },
    /// A routed remote-step batch targeted this daemon and should be executed locally.
    RemoteStepRequested {
        request_id: u64,
        requester_node_id: NodeId,
        reply_via: NodeId,
        repo_identity: RepoIdentity,
        step_offset: usize,
        steps: Vec<Step>,
    },
    /// A routed remote-step progress event reached the original requester.
    RemoteStepEventReceived {
        request_id: u64,
        responder_node_id: NodeId,
        batch_step_index: usize,
        batch_step_count: usize,
        description: String,
        status: StepStatus,
    },
    /// A routed remote-step response reached the original requester.
    RemoteStepResponseReceived { request_id: u64, responder_node_id: NodeId, outcomes: Vec<StepOutcome> },
    /// A routed remote-step cancel request targeted this daemon.
    RemoteStepCancelRequested { cancel_id: u64, requester_node_id: NodeId, reply_via: NodeId, remote_step_request_id: u64 },
    /// A routed remote-step cancel response reached the original requester.
    RemoteStepCancelResponseReceived { cancel_id: u64, responder_node_id: NodeId, error: Option<String> },
    /// Nothing to do (e.g. message from self).
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionDirection {
    Inbound,
    Outbound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionMeta {
    pub direction: ConnectionDirection,
    pub config_label: Option<ConfigLabel>,
    pub expected_peer: Option<NodeId>,
    pub config_backed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveConnection {
    generation: u64,
    meta: ConnectionMeta,
    session_id: Option<uuid::Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationResult {
    Accepted { generation: u64, displaced: Option<u64> },
    Rejected { reason: GoodbyeReason },
}

#[derive(Debug, Clone)]
pub struct InboundPeerEnvelope {
    pub msg: PeerWireMessage,
    pub connection_generation: u64,
    pub connection_peer: NodeId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteHop {
    pub next_hop: NodeId,
    pub next_hop_generation: u64,
    pub learned_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteState {
    pub primary: RouteHop,
    pub fallbacks: Vec<RouteHop>,
    pub candidates: Vec<RouteHop>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommandReversePathKey {
    pub request_id: u64,
    pub requester_node_id: NodeId,
    pub target_node_id: NodeId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReversePathHop {
    pub next_hop: NodeId,
    pub next_hop_generation: u64,
    pub learned_at: u64,
}

pub struct PendingPeerSend {
    pub target: NodeId,
    pub sender: Arc<dyn PeerSender>,
    pub msg: PeerWireMessage,
}

struct ConfiguredPeerTarget {
    expected_host_name: HostName,
    expected_node_id: Option<NodeId>,
    connection_address: String,
    transport: Box<dyn PeerTransport>,
}

#[derive(Debug, Clone, Default)]
struct PeerDialStatus {
    last_attempt: Option<DateTime<Utc>>,
    last_error: Option<String>,
    reconnect_backoff: Option<ReconnectBackoff>,
}

#[derive(Debug, Clone)]
struct ReconnectBackoff {
    attempt: u32,
    next_dial_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredPeerTargetInfo {
    pub label: ConfigLabel,
    pub expected_host_name: HostName,
    pub expected_node_id: Option<NodeId>,
}

#[derive(Debug)]
pub struct ConnectedConfiguredPeer {
    pub label: ConfigLabel,
    pub node: NodeInfo,
    pub generation: u64,
    pub inbound_rx: mpsc::Receiver<PeerWireMessage>,
}

pub async fn dispatch_pending_sends(pending_sends: Vec<PendingPeerSend>) {
    for pending in pending_sends {
        let msg_kind = peer_wire_message_kind(&pending.msg);
        if let Err(e) = pending.sender.send(pending.msg).await {
            warn!(peer = %pending.target, msg_kind, err = %e, "failed to dispatch queued peer message");
        }
    }
}

fn peer_wire_message_kind(msg: &PeerWireMessage) -> &'static str {
    match msg {
        PeerWireMessage::HostSummary(_) => "host_summary",
        PeerWireMessage::RouteAdvertisement { .. } => "route_advertisement",
        PeerWireMessage::Routed(msg) => match msg {
            RoutedPeerMessage::CommandRequest { .. } => "command_request",
            RoutedPeerMessage::CommandCancelRequest { .. } => "command_cancel_request",
            RoutedPeerMessage::CommandEvent { .. } => "command_event",
            RoutedPeerMessage::CommandResponse { .. } => "command_response",
            RoutedPeerMessage::CommandCancelResponse { .. } => "command_cancel_response",
            RoutedPeerMessage::RemoteStepRequest { .. } => "remote_step_request",
            RoutedPeerMessage::RemoteStepEvent { .. } => "remote_step_event",
            RoutedPeerMessage::RemoteStepResponse { .. } => "remote_step_response",
            RoutedPeerMessage::RemoteStepCancelRequest { .. } => "remote_step_cancel_request",
            RoutedPeerMessage::RemoteStepCancelResponse { .. } => "remote_step_cancel_response",
        },
        PeerWireMessage::Goodbye { .. } => "goodbye",
        PeerWireMessage::Ping { .. } => "ping",
        PeerWireMessage::Pong { .. } => "pong",
    }
}

#[derive(Debug, Clone)]
pub struct DisconnectPlan {
    pub was_active: bool,
}

/// Manages live connections and routing between peer hosts.
pub struct PeerManager {
    local_node_id: NodeId,
    configured_targets: HashMap<ConfigLabel, ConfiguredPeerTarget>,
    peer_dial_status: HashMap<ConfigLabel, PeerDialStatus>,
    senders: HashMap<NodeId, Arc<dyn PeerSender>>,
    active_connections: HashMap<NodeId, ActiveConnection>,
    displaced_senders: HashMap<(NodeId, u64), Arc<dyn PeerSender>>,
    reconnect_suppressed_until: HashMap<NodeId, Instant>,
    transport_peers: HashMap<ConfigLabel, NodeId>,
    /// Last node identity learned for each configured transport. Unlike
    /// `transport_peers`, this survives disconnect so topology diagnostics
    /// remain attributable while retries are failing.
    learned_transport_peers: HashMap<ConfigLabel, NodeId>,
    /// Self-declared display names learned from peer gossip, including nodes
    /// reached through another peer.
    learned_node_names: HashMap<NodeId, String>,
    generations: HashMap<NodeId, u64>,
    routes: HashMap<NodeId, RouteState>,
    /// TODO: expire abandoned reverse-path entries when routed replies time out
    /// instead of only clearing them on reply delivery or disconnect.
    command_reverse_paths: HashMap<CommandReversePathKey, ReversePathHop>,
    pending_sends: Vec<PendingPeerSend>,
    route_epoch: u64,
    request_id_counter: u64,
    peer_host_summaries: HashMap<EnvironmentId, HostSummary>,
}

impl PeerManager {
    const GOODBYE_RECONNECT_SUPPRESSION: Duration = Duration::from_secs(15);
    pub(crate) const DEFAULT_ROUTED_HOPS: u8 = 8;

    /// Create a new PeerManager with no peers.
    pub fn new(local_node_id: NodeId) -> Self {
        Self {
            local_node_id,
            configured_targets: HashMap::new(),
            peer_dial_status: HashMap::new(),
            senders: HashMap::new(),
            active_connections: HashMap::new(),
            displaced_senders: HashMap::new(),
            reconnect_suppressed_until: HashMap::new(),
            transport_peers: HashMap::new(),
            learned_transport_peers: HashMap::new(),
            learned_node_names: HashMap::new(),
            generations: HashMap::new(),
            routes: HashMap::new(),
            pending_sends: Vec::new(),
            route_epoch: 0,
            request_id_counter: 0,
            peer_host_summaries: HashMap::new(),
            command_reverse_paths: HashMap::new(),
        }
    }

    fn node_info_for(&self, node_id: &NodeId) -> NodeInfo {
        self.peer_host_summaries
            .values()
            .find(|summary| summary.node.node_id == *node_id)
            .map(|summary| summary.node.clone())
            .or_else(|| self.learned_node_names.get(node_id).map(|display_name| NodeInfo::new(node_id.clone(), display_name.clone())))
            .unwrap_or_else(|| NodeInfo::new(node_id.clone(), node_id.to_string()))
    }

    pub fn learn_node_info(&mut self, node: &NodeInfo) {
        self.learned_node_names.insert(node.node_id.clone(), node.display_name.clone());
    }

    /// Register a configured outbound connection target.
    pub fn add_configured_target(
        &mut self,
        label: ConfigLabel,
        expected_host_name: HostName,
        expected_node_id: Option<NodeId>,
        transport: Box<dyn PeerTransport>,
    ) {
        info!(target = %label.0, expected_host = %expected_host_name, expected_node_id = ?expected_node_id, "registered configured peer target");
        let connection_address = transport.connection_address();
        self.configured_targets.insert(label, ConfiguredPeerTarget { expected_host_name, expected_node_id, connection_address, transport });
    }

    pub fn note_dial_attempt(&mut self, label: &ConfigLabel) {
        self.peer_dial_status.entry(label.clone()).or_default().last_attempt = Some(Utc::now());
    }

    pub fn note_dial_result(&mut self, label: &ConfigLabel, result: Result<(), &str>) {
        let status = self.peer_dial_status.entry(label.clone()).or_default();
        let connected = result.is_ok();
        status.last_error = result.err().map(str::to_string);
        if connected {
            status.reconnect_backoff = None;
        }
    }

    pub fn note_reconnect_backoff(&mut self, label: &ConfigLabel, attempt: u32, delay: Duration) {
        self.peer_dial_status.entry(label.clone()).or_default().reconnect_backoff =
            Some(ReconnectBackoff { attempt, next_dial_at: Instant::now() + delay });
    }

    pub fn project_host_list(&self, response: &mut HostListResponse) {
        for (label, target) in &self.configured_targets {
            let node_id =
                self.transport_peers.get(label).or_else(|| self.learned_transport_peers.get(label)).or(target.expected_node_id.as_ref());
            let reconnect = self.peer_dial_status.get(label).and_then(|status| status.reconnect_backoff.as_ref()).map(|backoff| {
                let remaining = backoff.next_dial_at.saturating_duration_since(Instant::now());
                let next_dial_in_seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
                PeerReconnectStatus { attempt: backoff.attempt, next_dial_in_seconds }
            });
            let connected = self.transport_peers.contains_key(label);
            let connection_status = if connected {
                PeerConnectionState::Connected
            } else if reconnect.is_some() {
                PeerConnectionState::Reconnecting
            } else {
                PeerConnectionState::Disconnected
            };

            let mut matched_existing = false;
            for entry in response
                .hosts
                .iter_mut()
                .filter(|entry| node_id.is_some_and(|node_id| entry.node.as_ref().is_some_and(|node| node.node_id == *node_id)))
            {
                matched_existing = true;
                entry.configured = true;
                entry.connection_status = connection_status.clone();
                entry.reconnect = reconnect.clone();
            }
            if matched_existing {
                continue;
            }

            response.hosts.push(HostListEntry {
                environment_id: None,
                host_name: target.expected_host_name.clone(),
                node: node_id.map(|node_id| self.node_info_for(node_id)),
                is_local: false,
                configured: true,
                connection_status,
                reconnect,
                has_summary: false,
                repo_count: 0,
            });
        }

        response.hosts.sort_by(|left, right| {
            right
                .is_local
                .cmp(&left.is_local)
                .then_with(|| left.host_name.cmp(&right.host_name))
                .then_with(|| left.node.as_ref().map(|node| &node.node_id).cmp(&right.node.as_ref().map(|node| &node.node_id)))
        });
    }

    /// Register or replace a sender for a connected peer.
    pub fn register_sender(&mut self, name: NodeId, sender: Arc<dyn PeerSender>) {
        self.senders.insert(name, sender);
    }

    fn next_route_epoch(&mut self) -> u64 {
        self.route_epoch = self.route_epoch.saturating_add(1);
        self.route_epoch
    }

    pub fn next_request_id(&mut self) -> u64 {
        self.request_id_counter = self.request_id_counter.saturating_add(1);
        self.request_id_counter
    }

    pub fn current_generation(&self, name: &NodeId) -> Option<u64> {
        self.active_connections.get(name).map(|active| active.generation)
    }

    /// Return the session ID for a connected peer, if known.
    pub fn peer_session_id(&self, host: &NodeId) -> Option<uuid::Uuid> {
        self.active_connections.get(host).and_then(|active| active.session_id)
    }

    pub fn reconnect_suppressed_until(&mut self, name: &NodeId) -> Option<Instant> {
        match self.reconnect_suppressed_until.get(name).copied() {
            Some(deadline) if deadline > Instant::now() => Some(deadline),
            Some(_) => {
                self.reconnect_suppressed_until.remove(name);
                None
            }
            None => None,
        }
    }

    fn generation_is_current(&self, name: &NodeId, generation: u64) -> bool {
        // Validity keys on the ACTIVE connection, not the mint counter: the
        // counter must stay monotonic across disconnects (replicator
        // supervision dedupes on it), while envelopes and hops from a
        // disconnected generation must still be rejected.
        generation != 0 && self.active_connections.get(name).map(|active| active.generation) == Some(generation)
    }

    fn install_direct_route(&mut self, host: &NodeId, generation: u64) {
        let learned_epoch = self.next_route_epoch();
        self.routes.insert(host.clone(), RouteState {
            primary: RouteHop { next_hop: host.clone(), next_hop_generation: generation, learned_epoch },
            fallbacks: Vec::new(),
            candidates: Vec::new(),
        });
    }

    fn route_hop_is_live(&self, hop: &RouteHop) -> bool {
        self.generation_is_current(&hop.next_hop, hop.next_hop_generation) && self.senders.contains_key(&hop.next_hop)
    }

    fn retain_unique_hops(hops: &mut Vec<RouteHop>, next_hop: &NodeId) {
        hops.retain(|hop| hop.next_hop != *next_hop);
    }

    fn observe_route(&mut self, origin: &NodeId, via_peer: &NodeId, via_generation: u64) {
        let learned_epoch = self.next_route_epoch();
        let new_hop = RouteHop { next_hop: via_peer.clone(), next_hop_generation: via_generation, learned_epoch };

        let Some(mut route) = self.routes.remove(origin) else {
            self.routes.insert(origin.clone(), RouteState { primary: new_hop, fallbacks: Vec::new(), candidates: Vec::new() });
            return;
        };

        if route.primary.next_hop == *via_peer {
            route.primary = new_hop;
            self.routes.insert(origin.clone(), route);
            return;
        }

        Self::retain_unique_hops(&mut route.fallbacks, via_peer);
        Self::retain_unique_hops(&mut route.candidates, via_peer);

        if origin == via_peer {
            if self.route_hop_is_live(&route.primary) && route.primary.next_hop != *origin {
                Self::retain_unique_hops(&mut route.fallbacks, &route.primary.next_hop);
                route.fallbacks.push(route.primary.clone());
            }
            route.primary = new_hop;
            self.routes.insert(origin.clone(), route);
            return;
        }

        if route.primary.next_hop == *origin && self.route_hop_is_live(&route.primary) {
            route.fallbacks.push(new_hop);
            self.routes.insert(origin.clone(), route);
            return;
        }

        if self.route_hop_is_live(&route.primary) {
            Self::retain_unique_hops(&mut route.fallbacks, &route.primary.next_hop);
            route.fallbacks.push(route.primary.clone());
        }
        route.primary = new_hop;
        self.routes.insert(origin.clone(), route);
    }

    fn promote_route_after_disconnect(&mut self, origin: &NodeId) -> Option<RouteHop> {
        let mut route = self.routes.remove(origin)?;

        route.fallbacks.retain(|hop| self.route_hop_is_live(hop) && hop.next_hop != *origin);
        route.candidates.retain(|hop| self.route_hop_is_live(hop) && hop.next_hop != *origin);

        if self.route_hop_is_live(&route.primary) && route.primary.next_hop != *origin {
            let primary = route.primary.clone();
            self.routes.insert(origin.clone(), route);
            return Some(primary);
        }

        if let Some((idx, _)) = route.fallbacks.iter().enumerate().max_by_key(|(_, hop)| hop.learned_epoch) {
            let next = route.fallbacks.remove(idx);
            route.primary = next.clone();
            self.routes.insert(origin.clone(), route);
            return Some(next);
        }

        self.routes.remove(origin);
        None
    }

    fn winning_direction(&self, host: &NodeId) -> ConnectionDirection {
        if self.local_node_id.as_str() < host.as_str() {
            ConnectionDirection::Outbound
        } else {
            ConnectionDirection::Inbound
        }
    }

    fn candidate_matches_winning_direction(&self, host: &NodeId, meta: &ConnectionMeta) -> bool {
        meta.direction == self.winning_direction(host)
    }

    fn should_accept_candidate(&self, host: &NodeId, active: &ActiveConnection, candidate: &ConnectionMeta) -> bool {
        let active_matches = self.candidate_matches_winning_direction(host, &active.meta);
        let candidate_matches = self.candidate_matches_winning_direction(host, candidate);

        match (active_matches, candidate_matches) {
            (false, true) => true,
            (true, false) => false,
            _ => false,
        }
    }

    pub fn activate_connection(&mut self, host: NodeId, sender: Arc<dyn PeerSender>, meta: ConnectionMeta) -> ActivationResult {
        self.activate_connection_with_session(host, sender, meta, None)
    }

    pub fn activate_connection_with_session(
        &mut self,
        host: NodeId,
        sender: Arc<dyn PeerSender>,
        meta: ConnectionMeta,
        session_id: Option<uuid::Uuid>,
    ) -> ActivationResult {
        let displaced = if let Some(active) = self.active_connections.get(&host) {
            if !self.should_accept_candidate(&host, active, &meta) {
                return ActivationResult::Rejected { reason: GoodbyeReason::Superseded };
            }
            Some(active.generation)
        } else {
            None
        };

        let generation = self.generations.get(&host).copied().unwrap_or(0).saturating_add(1);
        self.generations.insert(host.clone(), generation);
        if let Some(displaced_generation) = displaced {
            if let Some(displaced_sender) = self.senders.get(&host).cloned() {
                self.displaced_senders.insert((host.clone(), displaced_generation), displaced_sender);
            }
        }
        self.senders.insert(host.clone(), sender);
        self.active_connections.insert(host.clone(), ActiveConnection { generation, meta: meta.clone(), session_id });
        self.install_direct_route(&host, generation);

        if let Some(label) = meta.config_label {
            self.transport_peers.insert(label.clone(), host.clone());
            self.learned_transport_peers.insert(label, host);
        }

        ActivationResult::Accepted { generation, displaced }
    }

    pub async fn handle_inbound(&mut self, env: InboundPeerEnvelope) -> HandleResult {
        if !self.generation_is_current(&env.connection_peer, env.connection_generation) {
            debug!(
                peer = %env.connection_peer,
                generation = env.connection_generation,
                "dropping stale-generation peer message"
            );
            return HandleResult::Ignored;
        }

        match env.msg {
            PeerWireMessage::HostSummary(mut summary) => {
                summary.node.node_id = env.connection_peer.clone();
                self.store_host_summary(summary);
                HandleResult::Ignored
            }
            PeerWireMessage::RouteAdvertisement { origin_node_id, origin_display_name, remaining_hops, mut visited } => {
                if origin_node_id == self.local_node_id || visited.contains(&self.local_node_id) {
                    return HandleResult::Ignored;
                }

                self.learned_node_names.insert(origin_node_id.clone(), origin_display_name.clone());
                self.observe_route(&origin_node_id, &env.connection_peer, env.connection_generation);

                if remaining_hops > 1 {
                    visited.push(self.local_node_id.clone());
                    let forwarded = PeerWireMessage::RouteAdvertisement {
                        origin_node_id,
                        origin_display_name,
                        remaining_hops: remaining_hops - 1,
                        visited: visited.clone(),
                    };
                    let targets: Vec<NodeId> =
                        self.senders.keys().filter(|peer| **peer != env.connection_peer && !visited.contains(peer)).cloned().collect();
                    for target in targets {
                        self.queue_send_to(&target, forwarded.clone());
                    }
                }
                HandleResult::Ignored
            }
            PeerWireMessage::Routed(msg) => self.handle_routed(env.connection_peer, env.connection_generation, msg),
            PeerWireMessage::Goodbye { reason } => match reason {
                GoodbyeReason::Superseded | GoodbyeReason::Shutdown => {
                    self.reconnect_suppressed_until
                        .insert(env.connection_peer.clone(), Instant::now() + Self::GOODBYE_RECONNECT_SUPPRESSION);
                    HandleResult::ReconnectSuppressed { peer: env.connection_peer }
                }
            },
            PeerWireMessage::Ping { timestamp } => {
                self.queue_send_to(&env.connection_peer, PeerWireMessage::Pong { timestamp });
                HandleResult::Ignored
            }
            PeerWireMessage::Pong { .. } => HandleResult::Ignored,
        }
    }

    fn handle_routed(&mut self, connection_peer: NodeId, connection_generation: u64, msg: RoutedPeerMessage) -> HandleResult {
        match msg {
            RoutedPeerMessage::CommandRequest { request_id, requester_node_id, target_node_id, remaining_hops, command, session_id } => {
                if remaining_hops == 0 {
                    return HandleResult::Ignored;
                }
                if target_node_id == self.local_node_id {
                    return HandleResult::CommandRequested {
                        request_id,
                        requester_node_id,
                        reply_via: connection_peer,
                        command: *command,
                        session_id,
                    };
                }

                let key = CommandReversePathKey {
                    request_id,
                    requester_node_id: requester_node_id.clone(),
                    target_node_id: target_node_id.clone(),
                };
                let learned_at = self.next_route_epoch();
                self.command_reverse_paths.insert(key, ReversePathHop {
                    next_hop: connection_peer,
                    next_hop_generation: connection_generation,
                    learned_at,
                });

                let forwarded = RoutedPeerMessage::CommandRequest {
                    request_id,
                    requester_node_id,
                    target_node_id: target_node_id.clone(),
                    remaining_hops: remaining_hops.saturating_sub(1),
                    command,
                    session_id,
                };
                self.queue_send_to(&target_node_id, PeerWireMessage::Routed(forwarded));
                HandleResult::Ignored
            }
            RoutedPeerMessage::CommandCancelRequest {
                cancel_id,
                requester_node_id,
                target_node_id,
                remaining_hops,
                command_request_id,
            } => {
                if remaining_hops == 0 {
                    return HandleResult::Ignored;
                }
                if target_node_id == self.local_node_id {
                    return HandleResult::CommandCancelRequested {
                        cancel_id,
                        requester_node_id,
                        reply_via: connection_peer,
                        command_request_id,
                    };
                }

                let key = CommandReversePathKey {
                    request_id: cancel_id,
                    requester_node_id: requester_node_id.clone(),
                    target_node_id: target_node_id.clone(),
                };
                let learned_at = self.next_route_epoch();
                self.command_reverse_paths.insert(key, ReversePathHop {
                    next_hop: connection_peer,
                    next_hop_generation: connection_generation,
                    learned_at,
                });

                let forwarded = RoutedPeerMessage::CommandCancelRequest {
                    cancel_id,
                    requester_node_id,
                    target_node_id: target_node_id.clone(),
                    remaining_hops: remaining_hops.saturating_sub(1),
                    command_request_id,
                };
                self.queue_send_to(&target_node_id, PeerWireMessage::Routed(forwarded));
                HandleResult::Ignored
            }
            RoutedPeerMessage::CommandEvent { request_id, requester_node_id, responder_node_id, remaining_hops, event } => {
                let key = CommandReversePathKey {
                    request_id,
                    requester_node_id: requester_node_id.clone(),
                    target_node_id: responder_node_id.clone(),
                };

                if requester_node_id == self.local_node_id {
                    return HandleResult::CommandEventReceived { request_id, responder_node_id, event: *event };
                }

                if remaining_hops == 0 {
                    return HandleResult::Ignored;
                }

                let Some(reverse_hop) = self.command_reverse_paths.get(&key).cloned() else {
                    return HandleResult::Ignored;
                };
                if !self.generation_is_current(&reverse_hop.next_hop, reverse_hop.next_hop_generation) {
                    self.command_reverse_paths.remove(&key);
                    return HandleResult::Ignored;
                }

                let forwarded = RoutedPeerMessage::CommandEvent {
                    request_id,
                    requester_node_id,
                    responder_node_id,
                    remaining_hops: remaining_hops.saturating_sub(1),
                    event,
                };
                if let Some(sender) = self.senders.get(&reverse_hop.next_hop).cloned() {
                    self.pending_sends.push(PendingPeerSend {
                        target: reverse_hop.next_hop.clone(),
                        sender,
                        msg: PeerWireMessage::Routed(forwarded),
                    });
                }
                HandleResult::Ignored
            }
            RoutedPeerMessage::CommandResponse { request_id, requester_node_id, responder_node_id, remaining_hops, result } => {
                let key = CommandReversePathKey {
                    request_id,
                    requester_node_id: requester_node_id.clone(),
                    target_node_id: responder_node_id.clone(),
                };

                if requester_node_id == self.local_node_id {
                    return HandleResult::CommandResponseReceived { request_id, responder_node_id, result: *result };
                }

                if remaining_hops == 0 {
                    return HandleResult::Ignored;
                }

                let Some(reverse_hop) = self.command_reverse_paths.get(&key).cloned() else {
                    return HandleResult::Ignored;
                };
                if !self.generation_is_current(&reverse_hop.next_hop, reverse_hop.next_hop_generation) {
                    self.command_reverse_paths.remove(&key);
                    return HandleResult::Ignored;
                }

                let forwarded = RoutedPeerMessage::CommandResponse {
                    request_id,
                    requester_node_id,
                    responder_node_id,
                    remaining_hops: remaining_hops.saturating_sub(1),
                    result,
                };
                if let Some(sender) = self.senders.get(&reverse_hop.next_hop).cloned() {
                    self.pending_sends.push(PendingPeerSend {
                        target: reverse_hop.next_hop.clone(),
                        sender,
                        msg: PeerWireMessage::Routed(forwarded),
                    });
                }
                self.command_reverse_paths.remove(&key);
                HandleResult::Ignored
            }
            RoutedPeerMessage::CommandCancelResponse { cancel_id, requester_node_id, responder_node_id, remaining_hops, error } => {
                let key = CommandReversePathKey {
                    request_id: cancel_id,
                    requester_node_id: requester_node_id.clone(),
                    target_node_id: responder_node_id.clone(),
                };

                if requester_node_id == self.local_node_id {
                    return HandleResult::CommandCancelResponseReceived { cancel_id, responder_node_id, error };
                }

                if remaining_hops == 0 {
                    return HandleResult::Ignored;
                }

                let Some(reverse_hop) = self.command_reverse_paths.get(&key).cloned() else {
                    return HandleResult::Ignored;
                };
                if !self.generation_is_current(&reverse_hop.next_hop, reverse_hop.next_hop_generation) {
                    self.command_reverse_paths.remove(&key);
                    return HandleResult::Ignored;
                }

                let forwarded = RoutedPeerMessage::CommandCancelResponse {
                    cancel_id,
                    requester_node_id,
                    responder_node_id,
                    remaining_hops: remaining_hops.saturating_sub(1),
                    error,
                };
                if let Some(sender) = self.senders.get(&reverse_hop.next_hop).cloned() {
                    self.pending_sends.push(PendingPeerSend {
                        target: reverse_hop.next_hop.clone(),
                        sender,
                        msg: PeerWireMessage::Routed(forwarded),
                    });
                }
                self.command_reverse_paths.remove(&key);
                HandleResult::Ignored
            }
            RoutedPeerMessage::RemoteStepRequest {
                request_id,
                requester_node_id,
                target_node_id,
                remaining_hops,
                repo_identity,
                step_offset,
                steps,
            } => {
                if remaining_hops == 0 {
                    return HandleResult::Ignored;
                }
                if target_node_id == self.local_node_id {
                    return HandleResult::RemoteStepRequested {
                        request_id,
                        requester_node_id,
                        reply_via: connection_peer,
                        repo_identity,
                        step_offset,
                        steps,
                    };
                }

                let key = CommandReversePathKey {
                    request_id,
                    requester_node_id: requester_node_id.clone(),
                    target_node_id: target_node_id.clone(),
                };
                let learned_at = self.next_route_epoch();
                self.command_reverse_paths.insert(key, ReversePathHop {
                    next_hop: connection_peer,
                    next_hop_generation: connection_generation,
                    learned_at,
                });

                let forwarded = RoutedPeerMessage::RemoteStepRequest {
                    request_id,
                    requester_node_id,
                    target_node_id: target_node_id.clone(),
                    remaining_hops: remaining_hops.saturating_sub(1),
                    repo_identity,
                    step_offset,
                    steps,
                };
                self.queue_send_to(&target_node_id, PeerWireMessage::Routed(forwarded));
                HandleResult::Ignored
            }
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
                let key = CommandReversePathKey {
                    request_id,
                    requester_node_id: requester_node_id.clone(),
                    target_node_id: responder_node_id.clone(),
                };

                if requester_node_id == self.local_node_id {
                    return HandleResult::RemoteStepEventReceived {
                        request_id,
                        responder_node_id,
                        batch_step_index,
                        batch_step_count,
                        description,
                        status,
                    };
                }

                if remaining_hops == 0 {
                    return HandleResult::Ignored;
                }

                let Some(reverse_hop) = self.command_reverse_paths.get(&key).cloned() else {
                    return HandleResult::Ignored;
                };
                if !self.generation_is_current(&reverse_hop.next_hop, reverse_hop.next_hop_generation) {
                    self.command_reverse_paths.remove(&key);
                    return HandleResult::Ignored;
                }

                let forwarded = RoutedPeerMessage::RemoteStepEvent {
                    request_id,
                    requester_node_id,
                    responder_node_id,
                    remaining_hops: remaining_hops.saturating_sub(1),
                    batch_step_index,
                    batch_step_count,
                    description,
                    status,
                };
                if let Some(sender) = self.senders.get(&reverse_hop.next_hop).cloned() {
                    self.pending_sends.push(PendingPeerSend {
                        target: reverse_hop.next_hop.clone(),
                        sender,
                        msg: PeerWireMessage::Routed(forwarded),
                    });
                }
                HandleResult::Ignored
            }
            RoutedPeerMessage::RemoteStepResponse { request_id, requester_node_id, responder_node_id, remaining_hops, outcomes } => {
                let key = CommandReversePathKey {
                    request_id,
                    requester_node_id: requester_node_id.clone(),
                    target_node_id: responder_node_id.clone(),
                };

                if requester_node_id == self.local_node_id {
                    return HandleResult::RemoteStepResponseReceived { request_id, responder_node_id, outcomes };
                }

                if remaining_hops == 0 {
                    return HandleResult::Ignored;
                }

                let Some(reverse_hop) = self.command_reverse_paths.get(&key).cloned() else {
                    return HandleResult::Ignored;
                };
                if !self.generation_is_current(&reverse_hop.next_hop, reverse_hop.next_hop_generation) {
                    self.command_reverse_paths.remove(&key);
                    return HandleResult::Ignored;
                }

                let forwarded = RoutedPeerMessage::RemoteStepResponse {
                    request_id,
                    requester_node_id,
                    responder_node_id,
                    remaining_hops: remaining_hops.saturating_sub(1),
                    outcomes,
                };
                if let Some(sender) = self.senders.get(&reverse_hop.next_hop).cloned() {
                    self.pending_sends.push(PendingPeerSend {
                        target: reverse_hop.next_hop.clone(),
                        sender,
                        msg: PeerWireMessage::Routed(forwarded),
                    });
                }
                self.command_reverse_paths.remove(&key);
                HandleResult::Ignored
            }
            RoutedPeerMessage::RemoteStepCancelRequest {
                cancel_id,
                requester_node_id,
                target_node_id,
                remaining_hops,
                remote_step_request_id,
            } => {
                if remaining_hops == 0 {
                    return HandleResult::Ignored;
                }
                if target_node_id == self.local_node_id {
                    return HandleResult::RemoteStepCancelRequested {
                        cancel_id,
                        requester_node_id,
                        reply_via: connection_peer,
                        remote_step_request_id,
                    };
                }

                let key = CommandReversePathKey {
                    request_id: cancel_id,
                    requester_node_id: requester_node_id.clone(),
                    target_node_id: target_node_id.clone(),
                };
                let learned_at = self.next_route_epoch();
                self.command_reverse_paths.insert(key, ReversePathHop {
                    next_hop: connection_peer,
                    next_hop_generation: connection_generation,
                    learned_at,
                });

                let forwarded = RoutedPeerMessage::RemoteStepCancelRequest {
                    cancel_id,
                    requester_node_id,
                    target_node_id: target_node_id.clone(),
                    remaining_hops: remaining_hops.saturating_sub(1),
                    remote_step_request_id,
                };
                self.queue_send_to(&target_node_id, PeerWireMessage::Routed(forwarded));
                HandleResult::Ignored
            }
            RoutedPeerMessage::RemoteStepCancelResponse { cancel_id, requester_node_id, responder_node_id, remaining_hops, error } => {
                let key = CommandReversePathKey {
                    request_id: cancel_id,
                    requester_node_id: requester_node_id.clone(),
                    target_node_id: responder_node_id.clone(),
                };

                if requester_node_id == self.local_node_id {
                    return HandleResult::RemoteStepCancelResponseReceived { cancel_id, responder_node_id, error };
                }

                if remaining_hops == 0 {
                    return HandleResult::Ignored;
                }

                let Some(reverse_hop) = self.command_reverse_paths.get(&key).cloned() else {
                    return HandleResult::Ignored;
                };
                if !self.generation_is_current(&reverse_hop.next_hop, reverse_hop.next_hop_generation) {
                    self.command_reverse_paths.remove(&key);
                    return HandleResult::Ignored;
                }

                let forwarded = RoutedPeerMessage::RemoteStepCancelResponse {
                    cancel_id,
                    requester_node_id,
                    responder_node_id,
                    remaining_hops: remaining_hops.saturating_sub(1),
                    error,
                };
                if let Some(sender) = self.senders.get(&reverse_hop.next_hop).cloned() {
                    self.pending_sends.push(PendingPeerSend {
                        target: reverse_hop.next_hop.clone(),
                        sender,
                        msg: PeerWireMessage::Routed(forwarded),
                    });
                }
                self.command_reverse_paths.remove(&key);
                HandleResult::Ignored
            }
        }
    }

    /// Accessor for the local host name.
    pub fn local_node_id(&self) -> &NodeId {
        &self.local_node_id
    }

    pub fn store_host_summary(&mut self, summary: HostSummary) {
        self.peer_host_summaries.insert(summary.environment_id.clone(), summary);
    }

    pub fn get_peer_host_summaries(&self) -> &HashMap<EnvironmentId, HostSummary> {
        &self.peer_host_summaries
    }

    pub fn node_for_host_environment(&self, environment_id: &EnvironmentId) -> Result<(NodeId, HostName), String> {
        let summary =
            self.peer_host_summaries.get(environment_id).ok_or_else(|| format!("no peer summary for host environment {environment_id}"))?;
        let host_name = summary.host_name.clone().unwrap_or_else(|| HostName::new(summary.node.display_name.clone()));
        Ok((summary.node.node_id.clone(), host_name))
    }

    pub fn topology_routes(&self) -> Vec<TopologyRoute> {
        let mut routes: Vec<_> = self
            .routes
            .iter()
            .map(|(target, route)| {
                let mut fallbacks: Vec<_> = route.fallbacks.iter().filter(|hop| self.route_hop_is_live(hop)).collect();
                fallbacks.sort_by_key(|hop| std::cmp::Reverse(hop.learned_epoch));
                TopologyRoute {
                    target: self.node_info_for(target),
                    next_hop: self.node_info_for(&route.primary.next_hop),
                    direct: route.primary.next_hop == *target,
                    connected: self.route_hop_is_live(&route.primary),
                    fallbacks: fallbacks.into_iter().map(|hop| self.node_info_for(&hop.next_hop)).collect(),
                    last_attempt: self
                        .configured_label_for_node(target)
                        .and_then(|label| self.peer_dial_status.get(label))
                        .and_then(|status| status.last_attempt),
                    last_error: self
                        .configured_label_for_node(target)
                        .and_then(|label| self.peer_dial_status.get(label))
                        .and_then(|status| status.last_error.clone()),
                }
            })
            .collect();
        routes.sort_by(|a, b| a.target.node_id.cmp(&b.target.node_id));
        routes
    }

    pub fn route_advertisements_for(&self, peer: &NodeId) -> Vec<PeerWireMessage> {
        std::iter::once(self.local_node_id.clone())
            .chain(self.routes.keys().filter(|target| *target != peer && *target != &self.local_node_id).cloned())
            .map(|origin_node_id| PeerWireMessage::RouteAdvertisement {
                origin_display_name: self.node_info_for(&origin_node_id).display_name,
                origin_node_id,
                remaining_hops: Self::DEFAULT_ROUTED_HOPS,
                visited: vec![self.local_node_id.clone()],
            })
            .collect()
    }

    fn configured_label_for_node(&self, node_id: &NodeId) -> Option<&ConfigLabel> {
        self.active_connections
            .get(node_id)
            .and_then(|active| active.meta.config_label.as_ref())
            .or_else(|| self.transport_peers.iter().find_map(|(label, node)| (node == node_id).then_some(label)))
            .or_else(|| self.learned_transport_peers.iter().find_map(|(label, node)| (node == node_id).then_some(label)))
            .or_else(|| {
                self.configured_targets
                    .iter()
                    .find_map(|(label, target)| (target.expected_node_id.as_ref() == Some(node_id)).then_some(label))
            })
    }

    /// Connect all registered peer transports and return inbound receivers.
    ///
    /// For each successfully connected peer, calls `subscribe()` to obtain the
    /// inbound message receiver. The caller should spawn forwarding tasks that
    /// feed these receivers into the shared inbound peer-message channel.
    pub async fn connect_all(&mut self) -> Vec<ConnectedConfiguredPeer> {
        let labels: Vec<ConfigLabel> = self.configured_targets.keys().cloned().collect();
        let mut receivers = Vec::new();
        for label in labels {
            self.note_dial_attempt(&label);
            let connect_result = if let Some(target) = self.configured_targets.get_mut(&label) {
                let transport = &mut target.transport;
                match transport.connect().await {
                    Ok(()) => {
                        let sender = transport.sender();
                        let subscribe_result = transport.subscribe().await;
                        let remote_node = transport.remote_node_info();
                        let remote_session_id = transport.remote_session_id();
                        Ok((sender, subscribe_result, remote_node, remote_session_id))
                    }
                    Err(e) => Err(e),
                }
            } else {
                continue;
            };

            match connect_result {
                Ok((sender, subscribe_result, remote_node, remote_session_id)) => {
                    let Some(remote_node) = remote_node else {
                        warn!(target = %label.0, "peer transport connected without remote node identity");
                        self.note_dial_result(&label, Err("peer transport connected without remote node identity"));
                        if let Some(target) = self.configured_targets.get_mut(&label) {
                            let _ = target.transport.disconnect().await;
                        }
                        continue;
                    };
                    let name = remote_node.node_id.clone();
                    self.learn_node_info(&remote_node);
                    info!(target = %label.0, peer = %name, "peer transport connected");
                    let mut generation = 0;
                    if let Some(sender) = sender {
                        let displaced = match self.activate_connection_with_session(
                            name.clone(),
                            sender,
                            ConnectionMeta {
                                direction: ConnectionDirection::Outbound,
                                config_label: Some(label.clone()),
                                expected_peer: Some(name.clone()),
                                config_backed: true,
                            },
                            remote_session_id,
                        ) {
                            ActivationResult::Accepted { generation: accepted, displaced: displaced_generation } => {
                                generation = accepted;
                                displaced_generation
                            }
                            ActivationResult::Rejected { .. } => {
                                let error = format!("connection for {name} lost duplicate arbitration");
                                self.note_dial_result(&label, Err(&error));
                                if let Some(target) = self.configured_targets.get_mut(&label) {
                                    let _ = target.transport.disconnect().await;
                                }
                                continue;
                            }
                        };
                        if let Some(displaced_generation) = displaced {
                            if let Some(displaced_sender) = self.take_displaced_sender(&name, displaced_generation) {
                                let _ = displaced_sender.retire(GoodbyeReason::Superseded).await;
                            }
                        }
                    }
                    match subscribe_result {
                        Ok(rx) => {
                            self.note_dial_result(&label, Ok(()));
                            receivers.push(ConnectedConfiguredPeer { label: label.clone(), node: remote_node, generation, inbound_rx: rx })
                        }
                        Err(e) => {
                            warn!(peer = %name, target = %label.0, err = %e, "failed to subscribe to peer");
                            self.note_dial_result(&label, Err(&e));
                            let _ = self.disconnect_peer(&name, generation);
                            if let Some(target) = self.configured_targets.get_mut(&label) {
                                let _ = target.transport.disconnect().await;
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(target = %label.0, err = %e, "failed to connect peer transport");
                    self.note_dial_result(&label, Err(&e));
                }
            }
        }
        receivers
    }

    /// Disconnect all registered peer transports.
    pub async fn disconnect_all(&mut self) {
        let labels: Vec<ConfigLabel> = self.configured_targets.keys().cloned().collect();
        for label in labels {
            if let Some(target) = self.configured_targets.get_mut(&label) {
                match target.transport.disconnect().await {
                    Ok(()) => {
                        info!(target = %label.0, "peer transport disconnected");
                    }
                    Err(e) => {
                        warn!(target = %label.0, err = %e, "failed to disconnect peer transport");
                    }
                }
            }
        }
    }

    pub fn configured_targets(&self) -> Vec<ConfiguredPeerTargetInfo> {
        let mut targets: Vec<_> = self
            .configured_targets
            .iter()
            .map(|(label, target)| ConfiguredPeerTargetInfo {
                label: label.clone(),
                expected_host_name: target.expected_host_name.clone(),
                expected_node_id: target.expected_node_id.clone(),
            })
            .collect();
        targets.sort_by(|a, b| a.label.0.cmp(&b.label.0));
        targets
    }

    pub fn configured_peers(&self) -> Vec<NodeInfo> {
        let mut peers: Vec<_> = self.transport_peers.values().map(|node_id| self.node_info_for(node_id)).collect();
        peers.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        peers.dedup_by(|a, b| a.node_id == b.node_id);
        peers
    }

    /// Return the currently addressable peers that have active senders.
    pub fn active_peers(&self) -> Vec<NodeId> {
        self.senders.keys().cloned().collect()
    }

    pub fn active_peer_senders(&self) -> Vec<(NodeId, Arc<dyn PeerSender>)> {
        self.senders.iter().map(|(name, sender)| (name.clone(), Arc::clone(sender))).collect()
    }

    /// Returns the sender for a peer only if the given generation matches
    /// the peer's current generation. Used by targeted sends to avoid
    /// sending to a connection that has been superseded.
    pub fn get_sender_if_current(&self, peer: &NodeId, generation: u64) -> Option<Arc<dyn PeerSender>> {
        if !self.generation_is_current(peer, generation) {
            return None;
        }
        self.senders.get(peer).cloned()
    }

    pub fn resolve_sender(&self, name: &NodeId) -> Result<Arc<dyn PeerSender>, String> {
        if let Some(sender) = self.senders.get(name) {
            return Ok(Arc::clone(sender));
        }

        let route = self.routes.get(name).ok_or_else(|| format!("unknown peer: {name}"))?;
        self.senders.get(&route.primary.next_hop).cloned().ok_or_else(|| format!("missing next hop sender: {}", route.primary.next_hop))
    }

    pub fn connection_address_for(&self, target: &NodeId, host: &HostName) -> String {
        let next_hop = if self.senders.contains_key(target) {
            target
        } else {
            self.routes.get(target).map(|route| &route.primary.next_hop).unwrap_or(target)
        };
        if let Some(configured) = self
            .active_connections
            .get(next_hop)
            .and_then(|active| active.meta.config_label.as_ref())
            .and_then(|label| self.configured_targets.get(label))
        {
            return configured.connection_address.clone();
        }
        if let Some(configured) = self
            .configured_targets
            .iter()
            .find_map(|(_, configured)| (configured.expected_node_id.as_ref() == Some(next_hop)).then_some(configured))
        {
            return configured.connection_address.clone();
        }
        self.configured_targets
            .values()
            .find_map(|configured| (configured.expected_host_name == *host).then(|| configured.connection_address.clone()))
            .unwrap_or_else(|| format!("peer://{next_hop}"))
    }

    pub fn take_displaced_sender(&mut self, name: &NodeId, generation: u64) -> Option<Arc<dyn PeerSender>> {
        self.displaced_senders.remove(&(name.clone(), generation))
    }

    /// Send a message to a specific peer by name.
    pub async fn send_to(&self, name: &NodeId, msg: PeerWireMessage) -> Result<(), String> {
        let sender = self.resolve_sender(name)?;
        sender.send(msg).await
    }

    fn queue_send_to(&mut self, name: &NodeId, msg: PeerWireMessage) {
        let msg_kind = peer_wire_message_kind(&msg);
        match self.resolve_sender(name) {
            Ok(sender) => self.pending_sends.push(PendingPeerSend { target: name.clone(), sender, msg }),
            Err(e) => warn!(peer = %name, msg_kind, err = %e, "failed to queue peer message"),
        }
    }

    pub fn take_pending_sends(&mut self) -> Vec<PendingPeerSend> {
        mem::take(&mut self.pending_sends)
    }

    /// Reconnect a specific configured target: disconnect, then connect + subscribe.
    pub async fn reconnect_target(&mut self, label: &ConfigLabel) -> Result<ConnectedConfiguredPeer, String> {
        let current_peer = self.transport_peers.get(label).cloned();
        if let Some(current_peer) = current_peer.as_ref() {
            if let Some(deadline) = self.reconnect_suppressed_until(current_peer) {
                return Err(format!("reconnect suppressed until {:?}", deadline));
            }
        }

        self.note_dial_attempt(label);
        let result = async {
            let target = self.configured_targets.get_mut(label).ok_or_else(|| format!("unknown configured target: {}", label.0))?;
            let transport = &mut target.transport;

            let _ = transport.disconnect().await;

            transport.connect().await?;
            let sender = transport.sender();
            let rx = transport.subscribe().await?;
            let remote_node = transport.remote_node_info();
            let remote_session_id = transport.remote_session_id();
            Ok::<_, String>((sender, rx, remote_node, remote_session_id))
        }
        .await;
        let (sender, rx, remote_node, remote_session_id) = match result {
            Ok(connection) => connection,
            Err(error) => {
                self.note_dial_result(label, Err(&error));
                return Err(error);
            }
        };

        let Some(remote_node) = remote_node else {
            let error = format!("configured target {} connected without remote node identity", label.0);
            self.note_dial_result(label, Err(&error));
            return Err(error);
        };
        let name = remote_node.node_id.clone();
        self.learn_node_info(&remote_node);

        let mut generation = 0;
        if let Some(sender) = sender {
            let displaced = match self.activate_connection_with_session(
                name.clone(),
                sender,
                ConnectionMeta {
                    direction: ConnectionDirection::Outbound,
                    config_label: Some(label.clone()),
                    expected_peer: Some(name.clone()),
                    config_backed: true,
                },
                remote_session_id,
            ) {
                ActivationResult::Accepted { generation: accepted, displaced: displaced_generation } => {
                    generation = accepted;
                    displaced_generation
                }
                ActivationResult::Rejected { .. } => {
                    if let Some(target) = self.configured_targets.get_mut(label) {
                        let _ = target.transport.disconnect().await;
                    }
                    let error = format!("connection for {name} lost duplicate arbitration");
                    self.note_dial_result(label, Err(&error));
                    return Err(error);
                }
            };
            if let Some(displaced_generation) = displaced {
                if let Some(displaced_sender) = self.take_displaced_sender(&name, displaced_generation) {
                    let _ = displaced_sender.retire(GoodbyeReason::Superseded).await;
                }
            }
        }

        self.note_dial_result(label, Ok(()));
        Ok(ConnectedConfiguredPeer { label: label.clone(), node: remote_node, generation, inbound_rx: rx })
    }

    /// Clear cached live-link metadata after a remote daemon restart.
    pub fn clear_peer_state_for_restart(&mut self, origin: &NodeId) {
        self.peer_host_summaries.retain(|_, summary| summary.node.node_id != *origin);
        info!(peer = %origin, "cleared stale peer state after restart");
    }

    pub fn disconnect_peer(&mut self, name: &NodeId, generation: u64) -> DisconnectPlan {
        if !self.generation_is_current(name, generation) {
            return DisconnectPlan { was_active: false };
        }

        self.senders.remove(name);
        self.active_connections.remove(name);
        // Deliberately NOT removing from self.generations: it is the
        // monotonic mint counter. Resetting it made every reconnect mint
        // generation 1 again, which the replicator supervisor rejected as
        // a duplicate — wedging replication (and heartbeats) after any
        // one-sided daemon restart.
        self.displaced_senders.retain(|(host, _), _| host != name);
        self.transport_peers.retain(|_, node_id| node_id != name);
        self.command_reverse_paths.retain(|_, hop| hop.next_hop != *name);
        self.peer_host_summaries.retain(|_, summary| summary.node.node_id != *name);
        self.routes.remove(name);
        let affected_routes: Vec<NodeId> =
            self.routes.iter().filter(|(_, route)| route.primary.next_hop == *name).map(|(target, _)| target.clone()).collect();
        for target in affected_routes {
            self.promote_route_after_disconnect(&target);
        }

        DisconnectPlan { was_active: true }
    }
}

#[cfg(test)]
mod tests;
