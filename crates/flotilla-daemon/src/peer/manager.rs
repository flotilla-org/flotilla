use std::{
    collections::HashMap,
    mem,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use flotilla_protocol::{
    Command, CommandPeerEvent, CommandValue, ConfigLabel, EnvironmentId, GoodbyeReason, HostName, HostSummary, NodeId, NodeInfo,
    PeerDataKind, PeerDataMessage, PeerWireMessage, ProviderData, RepoIdentity, RoutedPeerMessage, Step, StepOutcome, StepStatus,
    TopologyRoute, VectorClock,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::transport::{PeerSender, PeerTransport};

/// Generate a synthetic path for a remote-only repo.
///
/// Remote-only repos have no local filesystem path. This function produces
/// a deterministic `PathBuf` that serves as a stable key for tab identity
/// and repo tracking. When a peer reports a concrete checkout root we fold
/// it into the synthetic path for readability; otherwise we fall back to a
/// repo-identity-based key.
pub fn synthetic_repo_path(node_id: &NodeId, repo_identity: &RepoIdentity, host_repo_root: Option<&Path>) -> PathBuf {
    if let Some(host_repo_root) = host_repo_root {
        // Strip leading `/` from absolute paths to avoid double-slash in the
        // resulting string (e.g. `<remote>/desktop//home/...`).
        let stripped = host_repo_root.strip_prefix("/").unwrap_or(host_repo_root);
        return PathBuf::from(format!("<remote>/{}/{}", node_id, stripped.display()));
    }

    PathBuf::from(format!("<remote>/{}/{}/{}", node_id, repo_identity.authority, repo_identity.path))
}

/// Result of handling an inbound PeerDataMessage.
#[derive(Debug, PartialEq, Eq)]
pub enum HandleResult {
    /// Data was updated for this repo — caller should trigger re-merge.
    Updated(RepoIdentity),
    /// The sender is requesting a resync — caller should send a snapshot back.
    ResyncRequested { request_id: u64, requester_node_id: NodeId, reply_via: NodeId, repo: RepoIdentity, since_seq: u64 },
    /// Peer intentionally retired this connection; reconnect should be suppressed briefly.
    ReconnectSuppressed { peer: NodeId },
    /// A delta was received but cannot be applied (seq gap or not yet implemented).
    /// Caller should request a full resync from the origin.
    NeedsResync { from: NodeId, repo: RepoIdentity },
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
pub struct ReversePathKey {
    pub request_id: u64,
    pub requester_node_id: NodeId,
    pub target_node_id: NodeId,
    pub repo_identity: RepoIdentity,
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

#[derive(Debug, Clone)]
pub struct PendingResyncRequest {
    pub deadline_at: Instant,
}

pub struct PendingPeerSend {
    pub target: NodeId,
    pub sender: Arc<dyn PeerSender>,
    pub msg: PeerWireMessage,
}

struct ConfiguredPeerTarget {
    expected_host_name: HostName,
    expected_node_id: Option<NodeId>,
    transport: Box<dyn PeerTransport>,
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
        PeerWireMessage::Data(_) => "data",
        PeerWireMessage::HostSummary(_) => "host_summary",
        PeerWireMessage::Routed(msg) => match msg {
            RoutedPeerMessage::RequestResync { .. } => "request_resync",
            RoutedPeerMessage::ResyncSnapshot { .. } => "resync_snapshot",
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

/// Pre-computed overlay update to apply to InProcessDaemon after releasing the PeerManager lock.
///
/// For `SetProviders`, the caller resolves `identity` → local path at apply time
/// (not at computation time) so the path is always fresh, avoiding TOCTOU if a repo
/// is added or removed concurrently with the disconnect.
#[derive(Debug, Clone)]
pub enum OverlayUpdate {
    /// Update peer_providers for a repo with remaining peer data.
    /// The caller resolves `identity` to the current local path at apply time.
    /// `overlay_version` gates the apply — stale versions are rejected.
    SetProviders { identity: RepoIdentity, peers: Vec<(NodeInfo, ProviderData)>, overlay_version: u64 },
    /// Remove a virtual repo — no peers remain.
    RemoveRepo { identity: RepoIdentity, path: PathBuf },
}

#[derive(Debug, Clone)]
pub struct DisconnectPlan {
    pub was_active: bool,
    pub affected_repos: Vec<RepoIdentity>,
    pub resync_requests: Vec<RoutedPeerMessage>,
    /// Pre-computed overlay state for each affected repo, captured atomically
    /// with the disconnect under the same lock.
    pub overlay_updates: Vec<OverlayUpdate>,
}

/// Per-repo state received from a single peer host.
pub struct PerRepoPeerState {
    pub provider_data: ProviderData,
    pub host_repo_root: Option<PathBuf>,
    pub seq: u64,
    pub via_peer: NodeId,
    pub via_generation: u64,
    pub stale: bool,
}

/// Manages connections to remote peer hosts and stores their provider data.
///
/// The PeerManager does NOT own the InProcessDaemon. It returns information
/// about what changed so the caller (DaemonServer / wiring code) can trigger
/// re-merge on the daemon.
pub struct PeerManager {
    local_node_id: NodeId,
    configured_targets: HashMap<ConfigLabel, ConfiguredPeerTarget>,
    senders: HashMap<NodeId, Arc<dyn PeerSender>>,
    active_connections: HashMap<NodeId, ActiveConnection>,
    displaced_senders: HashMap<(NodeId, u64), Arc<dyn PeerSender>>,
    reconnect_suppressed_until: HashMap<NodeId, Instant>,
    transport_peers: HashMap<ConfigLabel, NodeId>,
    generations: HashMap<NodeId, u64>,
    routes: HashMap<NodeId, RouteState>,
    /// TODO: expire abandoned reverse-path entries when routed replies time out
    /// instead of only clearing them on reply delivery or disconnect.
    reverse_paths: HashMap<ReversePathKey, ReversePathHop>,
    command_reverse_paths: HashMap<CommandReversePathKey, ReversePathHop>,
    /// TODO: sweep overdue requests by deadline_at; today these are removed on
    /// reply, targeted disconnect, or process restart.
    pending_resync_requests: HashMap<ReversePathKey, PendingResyncRequest>,
    pending_sends: Vec<PendingPeerSend>,
    route_epoch: u64,
    request_id_counter: u64,
    peer_data: HashMap<NodeId, HashMap<RepoIdentity, PerRepoPeerState>>,
    peer_host_summaries: HashMap<EnvironmentId, HostSummary>,
    /// RepoIdentity values that exist only on remote peers — no local repo
    /// matches. Each maps to the synthetic path used for tab identity.
    known_remote_repos: HashMap<RepoIdentity, PathBuf>,
    /// Last-seen vector clock per (origin_host, repo_identity) — used to
    /// detect and drop duplicate / already-seen messages.
    last_seen_clocks: HashMap<(NodeId, RepoIdentity), VectorClock>,
    /// Monotonic counter incremented on every peer-data mutation. Callers
    /// pass this version into `set_peer_providers` so stale applies (from
    /// a read-then-apply that lost the race) are rejected.
    overlay_version: u64,
}

impl PeerManager {
    const RESYNC_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
    const GOODBYE_RECONNECT_SUPPRESSION: Duration = Duration::from_secs(15);
    pub(crate) const DEFAULT_ROUTED_HOPS: u8 = 8;

    /// Create a new PeerManager with no peers.
    pub fn new(local_node_id: NodeId) -> Self {
        Self {
            local_node_id,
            configured_targets: HashMap::new(),
            senders: HashMap::new(),
            active_connections: HashMap::new(),
            displaced_senders: HashMap::new(),
            reconnect_suppressed_until: HashMap::new(),
            transport_peers: HashMap::new(),
            generations: HashMap::new(),
            routes: HashMap::new(),
            reverse_paths: HashMap::new(),
            pending_resync_requests: HashMap::new(),
            pending_sends: Vec::new(),
            route_epoch: 0,
            request_id_counter: 0,
            peer_data: HashMap::new(),
            peer_host_summaries: HashMap::new(),
            known_remote_repos: HashMap::new(),
            command_reverse_paths: HashMap::new(),
            last_seen_clocks: HashMap::new(),
            overlay_version: 0,
        }
    }

    /// Current overlay version. Callers read this while holding the PM lock
    /// and pass it to `set_peer_providers` so stale applies are rejected.
    pub fn overlay_version(&self) -> u64 {
        self.overlay_version
    }

    fn bump_overlay_version(&mut self) -> u64 {
        self.overlay_version += 1;
        self.overlay_version
    }

    fn node_info_for(&self, node_id: &NodeId) -> NodeInfo {
        self.peer_host_summaries
            .values()
            .find(|summary| summary.node.node_id == *node_id)
            .map(|summary| summary.node.clone())
            .unwrap_or_else(|| NodeInfo::new(node_id.clone(), node_id.to_string()))
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
        self.configured_targets.insert(label, ConfiguredPeerTarget { expected_host_name, expected_node_id, transport });
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

    pub fn note_pending_resync_request(&mut self, target_node_id: NodeId, repo_identity: RepoIdentity) -> u64 {
        let request_id = self.next_request_id();
        self.pending_resync_requests.insert(
            ReversePathKey { request_id, requester_node_id: self.local_node_id.clone(), target_node_id, repo_identity },
            PendingResyncRequest { deadline_at: Instant::now() + Self::RESYNC_REQUEST_TIMEOUT },
        );
        request_id
    }

    pub fn sweep_expired_resyncs(&mut self, now: Instant) -> Vec<RepoIdentity> {
        let expired: Vec<ReversePathKey> =
            self.pending_resync_requests.iter().filter(|(_, pending)| pending.deadline_at <= now).map(|(key, _)| key.clone()).collect();

        let mut affected_repos = Vec::new();
        for key in expired {
            self.pending_resync_requests.remove(&key);
            let origin = key.target_node_id.clone();
            let repo = key.repo_identity.clone();

            let removed = if let Some(repos) = self.peer_data.get_mut(&origin) {
                if let Some(state) = repos.get(&repo) {
                    if state.stale {
                        repos.remove(&repo);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

            if removed {
                self.last_seen_clocks.remove(&(origin.clone(), repo.clone()));
                if self.peer_data.get(&origin).is_some_and(|repos| repos.is_empty()) {
                    self.peer_data.remove(&origin);
                }
                affected_repos.push(repo);
            }
        }

        if !affected_repos.is_empty() {
            self.bump_overlay_version();
        }

        affected_repos
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
        generation != 0 && self.generations.get(name).copied() == Some(generation)
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
            self.transport_peers.insert(label, host);
        }

        ActivationResult::Accepted { generation, displaced }
    }

    fn store_snapshot_from(&mut self, via_peer: &NodeId, via_generation: u64, msg: PeerDataMessage) -> HandleResult {
        let origin = msg.origin_node_id.clone();
        let repo = msg.repo_identity.clone();
        let host_repo_root = msg.host_repo_root.clone();

        let dedup_key = (origin.clone(), repo.clone());
        if let Some(last_seen) = self.last_seen_clocks.get(&dedup_key) {
            if msg.clock.dominated_by(last_seen) {
                debug!(
                    origin = %origin,
                    repo = %repo,
                    "dropping duplicate peer message (clock dominated)"
                );
                return HandleResult::Ignored;
            }
        }

        self.last_seen_clocks.entry(dedup_key).or_default().merge(&msg.clock);

        match msg.kind {
            PeerDataKind::Snapshot { data, seq } => {
                let repo_states = self.peer_data.entry(origin.clone()).or_default();
                repo_states.insert(repo.clone(), PerRepoPeerState {
                    provider_data: *data,
                    host_repo_root,
                    seq,
                    via_peer: via_peer.clone(),
                    via_generation,
                    stale: false,
                });

                self.observe_route(&origin, via_peer, via_generation);
                self.bump_overlay_version();

                HandleResult::Updated(repo)
            }
            PeerDataKind::Delta { seq, prev_seq, changes: _ } => {
                debug!(
                    origin = %origin,
                    repo = %repo,
                    %seq,
                    %prev_seq,
                    "received peer delta, requesting resync (delta application not yet implemented)"
                );

                HandleResult::NeedsResync { from: origin, repo }
            }
            PeerDataKind::RequestResync { .. } => {
                debug!(
                    origin = %origin,
                    repo = %repo,
                    "ignoring legacy direct peer request-resync message"
                );
                HandleResult::Ignored
            }
        }
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
            PeerWireMessage::Data(msg) => {
                if msg.origin_node_id == self.local_node_id {
                    debug!(node = %msg.origin_node_id, "ignoring peer data from self");
                    return HandleResult::Ignored;
                }
                self.store_snapshot_from(&env.connection_peer, env.connection_generation, msg)
            }
            PeerWireMessage::HostSummary(mut summary) => {
                summary.node.node_id = env.connection_peer.clone();
                self.store_host_summary(summary);
                HandleResult::Ignored
            }
            PeerWireMessage::Routed(msg) => self.handle_routed(env.connection_peer, env.connection_generation, msg),
            PeerWireMessage::Goodbye { reason } => match reason {
                GoodbyeReason::Superseded => {
                    self.reconnect_suppressed_until
                        .insert(env.connection_peer.clone(), Instant::now() + Self::GOODBYE_RECONNECT_SUPPRESSION);
                    HandleResult::ReconnectSuppressed { peer: env.connection_peer }
                }
            },
            PeerWireMessage::Ping { .. } | PeerWireMessage::Pong { .. } => HandleResult::Ignored,
        }
    }

    fn handle_routed(&mut self, connection_peer: NodeId, connection_generation: u64, msg: RoutedPeerMessage) -> HandleResult {
        match msg {
            RoutedPeerMessage::RequestResync {
                request_id,
                requester_node_id,
                target_node_id,
                remaining_hops,
                repo_identity,
                since_seq,
            } => {
                if remaining_hops == 0 {
                    return HandleResult::Ignored;
                }
                if target_node_id == self.local_node_id {
                    return HandleResult::ResyncRequested {
                        request_id,
                        requester_node_id,
                        reply_via: connection_peer,
                        repo: repo_identity,
                        since_seq,
                    };
                }

                let key = ReversePathKey {
                    request_id,
                    requester_node_id: requester_node_id.clone(),
                    target_node_id: target_node_id.clone(),
                    repo_identity: repo_identity.clone(),
                };
                let learned_at = self.next_route_epoch();
                self.reverse_paths.insert(key, ReversePathHop {
                    next_hop: connection_peer,
                    next_hop_generation: connection_generation,
                    learned_at,
                });

                let forwarded = RoutedPeerMessage::RequestResync {
                    request_id,
                    requester_node_id,
                    target_node_id: target_node_id.clone(),
                    remaining_hops: remaining_hops.saturating_sub(1),
                    repo_identity,
                    since_seq,
                };
                self.queue_send_to(&target_node_id, PeerWireMessage::Routed(forwarded));
                HandleResult::Ignored
            }
            RoutedPeerMessage::ResyncSnapshot {
                request_id,
                requester_node_id,
                responder_node_id,
                remaining_hops,
                repo_identity,
                host_repo_root,
                clock,
                seq,
                data,
            } => {
                let key = ReversePathKey {
                    request_id,
                    requester_node_id: requester_node_id.clone(),
                    target_node_id: responder_node_id.clone(),
                    repo_identity: repo_identity.clone(),
                };

                if requester_node_id == self.local_node_id {
                    if self.pending_resync_requests.remove(&key).is_none() {
                        return HandleResult::Ignored;
                    }
                    self.last_seen_clocks.remove(&(responder_node_id.clone(), repo_identity.clone()));
                    return self.store_snapshot_from(&connection_peer, connection_generation, PeerDataMessage {
                        origin_node_id: responder_node_id,
                        repo_identity,
                        host_repo_root,
                        clock,
                        kind: PeerDataKind::Snapshot { data, seq },
                    });
                }

                if remaining_hops == 0 {
                    return HandleResult::Ignored;
                }

                let Some(reverse_hop) = self.reverse_paths.get(&key).cloned() else {
                    return HandleResult::Ignored;
                };
                if !self.generation_is_current(&reverse_hop.next_hop, reverse_hop.next_hop_generation) {
                    self.reverse_paths.remove(&key);
                    return HandleResult::Ignored;
                }

                let forwarded = RoutedPeerMessage::ResyncSnapshot {
                    request_id,
                    requester_node_id,
                    responder_node_id,
                    remaining_hops: remaining_hops.saturating_sub(1),
                    repo_identity,
                    host_repo_root,
                    clock,
                    seq,
                    data,
                };
                if let Some(sender) = self.senders.get(&reverse_hop.next_hop).cloned() {
                    self.pending_sends.push(PendingPeerSend {
                        target: reverse_hop.next_hop.clone(),
                        sender,
                        msg: PeerWireMessage::Routed(forwarded),
                    });
                }
                self.reverse_paths.remove(&key);
                HandleResult::Ignored
            }
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

    /// Forward a message to all connected peers except the origin, self,
    /// and any host already present in the message's vector clock (which
    /// indicates that host has already seen or relayed the message).
    pub async fn relay(&self, origin: &NodeId, msg: &PeerDataMessage) {
        // Stamp our own host into the clock before relaying
        let mut relayed_msg = msg.clone();
        relayed_msg.clock.tick(&self.local_node_id);

        for (name, sender) in &self.senders {
            if name == origin || name == &self.local_node_id {
                continue;
            }
            // Skip peers that already appear in the clock — they've
            // already seen or relayed this message.
            if msg.clock.get(name) > 0 {
                debug!(
                    to = %name,
                    repo = %msg.repo_identity,
                    "skipping relay to peer already in clock"
                );
                continue;
            }

            match sender.send(PeerWireMessage::Data(relayed_msg.clone())).await {
                Ok(()) => {
                    debug!(
                        from = %origin,
                        to = %name,
                        repo = %msg.repo_identity,
                        "relayed peer data"
                    );
                }
                Err(e) => {
                    warn!(
                        from = %origin,
                        to = %name,
                        err = %e,
                        "failed to relay peer data"
                    );
                }
            }
        }
    }

    /// Accessor for the local host name.
    pub fn local_node_id(&self) -> &NodeId {
        &self.local_node_id
    }

    /// Accessor for all stored peer data — used by the merge layer.
    pub fn get_peer_data(&self) -> &HashMap<NodeId, HashMap<RepoIdentity, PerRepoPeerState>> {
        &self.peer_data
    }

    pub fn store_host_summary(&mut self, summary: HostSummary) {
        self.peer_host_summaries.insert(summary.environment_id.clone(), summary);
    }

    pub fn get_peer_host_summaries(&self) -> &HashMap<EnvironmentId, HostSummary> {
        &self.peer_host_summaries
    }

    pub fn topology_routes(&self) -> Vec<TopologyRoute> {
        let mut routes: Vec<_> = self
            .routes
            .iter()
            .map(|(target, route)| TopologyRoute {
                target: self.node_info_for(target),
                next_hop: self.node_info_for(&route.primary.next_hop),
                direct: route.primary.next_hop == *target,
                connected: self.route_hop_is_live(&route.primary),
                fallbacks: route
                    .fallbacks
                    .iter()
                    .filter(|hop| self.route_hop_is_live(hop))
                    .map(|hop| self.node_info_for(&hop.next_hop))
                    .collect(),
            })
            .collect();
        routes.sort_by(|a, b| a.target.node_id.cmp(&b.target.node_id));
        routes
    }

    /// Snapshot relay targets without performing any async sends.
    ///
    /// Returns a list of `(target, sender, stamped message)` tuples for peers
    /// that should receive the relayed message. The caller sends concurrently
    /// outside the PeerManager lock, eliminating head-of-line blocking.
    pub fn prepare_relay(&self, origin: &NodeId, msg: &PeerDataMessage) -> Vec<(NodeId, Arc<dyn PeerSender>, PeerDataMessage)> {
        let mut relayed_msg = msg.clone();
        relayed_msg.clock.tick(&self.local_node_id);

        self.senders
            .iter()
            .filter(|(name, _)| *name != origin && *name != &self.local_node_id && msg.clock.get(name) == 0)
            .map(|(name, sender)| (name.clone(), Arc::clone(sender), relayed_msg.clone()))
            .collect()
    }

    /// Connect all registered peer transports and return inbound receivers.
    ///
    /// For each successfully connected peer, calls `subscribe()` to obtain the
    /// inbound message receiver. The caller should spawn forwarding tasks that
    /// feed these receivers into the shared `peer_data_tx` channel.
    pub async fn connect_all(&mut self) -> Vec<ConnectedConfiguredPeer> {
        let labels: Vec<ConfigLabel> = self.configured_targets.keys().cloned().collect();
        let mut receivers = Vec::new();
        for label in labels {
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
                        if let Some(target) = self.configured_targets.get_mut(&label) {
                            let _ = target.transport.disconnect().await;
                        }
                        continue;
                    };
                    let name = remote_node.node_id.clone();
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
                            receivers.push(ConnectedConfiguredPeer { label: label.clone(), node: remote_node, generation, inbound_rx: rx })
                        }
                        Err(e) => {
                            warn!(peer = %name, target = %label.0, err = %e, "failed to subscribe to peer");
                        }
                    }
                }
                Err(e) => {
                    warn!(target = %label.0, err = %e, "failed to connect peer transport");
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

    /// Register a repo identity as remote-only.
    ///
    /// Called by the wiring layer after determining that a peer's repo has
    /// no matching local repo. The `synthetic_path` is used as the stable
    /// key for tab identity in the TUI.
    pub fn register_remote_repo(&mut self, identity: RepoIdentity, synthetic_path: PathBuf) {
        info!(repo = %identity, path = %synthetic_path.display(), "registered remote-only repo");
        self.known_remote_repos.insert(identity, synthetic_path);
    }

    /// Check whether a repo identity is known to be remote-only.
    pub fn is_remote_repo(&self, identity: &RepoIdentity) -> bool {
        self.known_remote_repos.contains_key(identity)
    }

    /// Accessor for all known remote-only repos and their synthetic paths.
    pub fn known_remote_repos(&self) -> &HashMap<RepoIdentity, PathBuf> {
        &self.known_remote_repos
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

    pub fn take_displaced_sender(&mut self, name: &NodeId, generation: u64) -> Option<Arc<dyn PeerSender>> {
        self.displaced_senders.remove(&(name.clone(), generation))
    }

    /// Remove all stored data for a peer (e.g. on disconnect).
    ///
    /// Returns the list of RepoIdentity values that were affected, so the
    /// caller can rebuild the daemon's peer overlay for those repos.
    pub fn remove_peer_data(&mut self, name: &NodeId) -> Vec<RepoIdentity> {
        let affected: Vec<RepoIdentity> = self.peer_data.get(name).map(|repos| repos.keys().cloned().collect()).unwrap_or_default();
        self.peer_data.remove(name);
        self.peer_host_summaries.retain(|_, summary| summary.node.node_id != *name);
        self.last_seen_clocks.retain(|(host, _), _| host != name);
        info!(peer = %name, repos = affected.len(), "cleared peer data");
        affected
    }

    /// Check whether a remote-only repo still has any peer data backing it.
    ///
    /// Returns `true` if at least one remaining peer holds data for this identity.
    pub fn has_peer_data_for(&self, identity: &RepoIdentity) -> bool {
        self.peer_data.values().any(|repos| repos.contains_key(identity))
    }

    /// Unregister a remote-only repo identity.
    ///
    /// Returns the synthetic path if it was tracked, so the caller can
    /// call `remove_repo` on the daemon.
    pub fn unregister_remote_repo(&mut self, identity: &RepoIdentity) -> Option<PathBuf> {
        self.known_remote_repos.remove(identity)
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

        let (sender, rx, remote_node, remote_session_id) = {
            let target = self.configured_targets.get_mut(label).ok_or_else(|| format!("unknown configured target: {}", label.0))?;
            let transport = &mut target.transport;

            let _ = transport.disconnect().await;

            transport.connect().await?;
            let sender = transport.sender();
            let rx = transport.subscribe().await?;
            let remote_node = transport.remote_node_info();
            let remote_session_id = transport.remote_session_id();
            (sender, rx, remote_node, remote_session_id)
        };

        let remote_node = remote_node.ok_or_else(|| format!("configured target {} connected without remote node identity", label.0))?;
        let name = remote_node.node_id.clone();

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
                    return Err(format!("connection for {name} lost duplicate arbitration"));
                }
            };
            if let Some(displaced_generation) = displaced {
                if let Some(displaced_sender) = self.take_displaced_sender(&name, displaced_generation) {
                    let _ = displaced_sender.retire(GoodbyeReason::Superseded).await;
                }
            }
        }

        Ok(ConnectedConfiguredPeer { label: label.clone(), node: remote_node, generation, inbound_rx: rx })
    }

    /// Clear all stored peer data originating from a specific host.
    ///
    /// Used when a remote daemon restart is detected (session_id changed).
    /// Unlike `disconnect_peer`, this does NOT tear down the connection.
    pub fn clear_peer_data_for_restart(&mut self, origin: &NodeId) -> Vec<RepoIdentity> {
        let Some(repos) = self.peer_data.remove(origin) else {
            // Restart cleanup still owns host-summary eviction even when no repo snapshots were cached.
            self.peer_host_summaries.retain(|_, summary| summary.node.node_id != *origin);
            return Vec::new();
        };
        let affected: Vec<RepoIdentity> = repos.keys().cloned().collect();
        self.peer_host_summaries.retain(|_, summary| summary.node.node_id != *origin);
        self.last_seen_clocks.retain(|(host, _), _| host != origin);
        if !affected.is_empty() {
            self.bump_overlay_version();
        }
        info!(peer = %origin, repo_count = affected.len(), "cleared stale peer data after restart");
        affected
    }

    pub fn disconnect_peer(&mut self, name: &NodeId, generation: u64) -> DisconnectPlan {
        if !self.generation_is_current(name, generation) {
            return DisconnectPlan {
                was_active: false,
                affected_repos: Vec::new(),
                resync_requests: Vec::new(),
                overlay_updates: Vec::new(),
            };
        }

        self.senders.remove(name);
        self.active_connections.remove(name);
        self.generations.remove(name);
        self.displaced_senders.retain(|(host, _), _| host != name);
        self.transport_peers.retain(|_, node_id| node_id != name);
        self.reverse_paths.retain(|_, hop| hop.next_hop != *name);
        self.command_reverse_paths.retain(|_, hop| hop.next_hop != *name);
        self.pending_resync_requests.retain(|key, _| key.target_node_id != *name);
        self.peer_host_summaries.retain(|_, summary| summary.node.node_id != *name);

        let mut affected_repos = Vec::new();
        let mut resync_requests = Vec::new();
        let origins: Vec<NodeId> = self.peer_data.keys().cloned().collect();

        for origin in origins {
            let affected_for_origin: Vec<RepoIdentity> = self
                .peer_data
                .get(&origin)
                .map(|repos| {
                    repos
                        .iter()
                        .filter(|(_, state)| state.via_peer == *name && state.via_generation == generation)
                        .map(|(repo_id, _)| repo_id.clone())
                        .collect()
                })
                .unwrap_or_default();

            if affected_for_origin.is_empty() {
                continue;
            }

            let replacement = self.promote_route_after_disconnect(&origin);
            if let Some(next_hop) = replacement {
                if let Some(repos) = self.peer_data.get_mut(&origin) {
                    for repo_id in &affected_for_origin {
                        if let Some(state) = repos.get_mut(repo_id) {
                            state.stale = true;
                            state.via_peer = next_hop.next_hop.clone();
                            state.via_generation = next_hop.next_hop_generation;
                        }
                    }
                }

                for repo_id in &affected_for_origin {
                    let request_id = self.next_request_id();
                    let key = ReversePathKey {
                        request_id,
                        requester_node_id: self.local_node_id.clone(),
                        target_node_id: origin.clone(),
                        repo_identity: repo_id.clone(),
                    };
                    self.pending_resync_requests
                        .insert(key, PendingResyncRequest { deadline_at: Instant::now() + Self::RESYNC_REQUEST_TIMEOUT });
                    resync_requests.push(RoutedPeerMessage::RequestResync {
                        request_id,
                        requester_node_id: self.local_node_id.clone(),
                        target_node_id: origin.clone(),
                        remaining_hops: Self::DEFAULT_ROUTED_HOPS,
                        repo_identity: repo_id.clone(),
                        since_seq: 0,
                    });
                }

                debug!(
                    origin = %origin,
                    via = %next_hop.next_hop,
                    repos = affected_for_origin.len(),
                    "retaining stale peer data while failover resync is pending"
                );
            } else {
                if let Some(repos) = self.peer_data.get_mut(&origin) {
                    for repo_id in &affected_for_origin {
                        repos.remove(repo_id);
                    }
                    if repos.is_empty() {
                        self.peer_data.remove(&origin);
                    }
                }
                self.routes.remove(&origin);
            }

            affected_repos.extend(affected_for_origin);
        }

        self.last_seen_clocks.retain(|(host, _), _| host != name);

        // Compute overlay updates atomically while still holding &mut self.
        // The caller resolves identity → path at apply time to avoid TOCTOU
        // with concurrent add_repo/remove_repo.
        //
        // Bump the overlay version once for the entire disconnect. All
        // SetProviders updates carry this version so stale applies are rejected.
        let overlay_version = if !affected_repos.is_empty() { self.bump_overlay_version() } else { self.overlay_version };
        let mut overlay_updates = Vec::new();
        for repo_id in &affected_repos {
            if self.has_peer_data_for(repo_id) {
                // Repo still has data from other peers — collect remaining peer data
                let peers: Vec<(NodeInfo, ProviderData)> = self
                    .peer_data
                    .iter()
                    .filter_map(|(node_id, repos)| {
                        repos.get(repo_id).map(|state| (self.node_info_for(node_id), state.provider_data.clone()))
                    })
                    .collect();
                overlay_updates.push(OverlayUpdate::SetProviders { identity: repo_id.clone(), peers, overlay_version });
            } else if let Some(synthetic_path) = self.unregister_remote_repo(repo_id) {
                // Remote-only, no peers remain — remove the virtual tab
                overlay_updates.push(OverlayUpdate::RemoveRepo { identity: repo_id.clone(), path: synthetic_path });
            }
        }

        DisconnectPlan { was_active: true, affected_repos, resync_requests, overlay_updates }
    }
}

#[cfg(test)]
mod tests;
