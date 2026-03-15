use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use flotilla_protocol::{
    Command, CommandPeerEvent, CommandResult, ConfigLabel, GoodbyeReason, HostName, HostSummary, PeerDataKind, PeerDataMessage,
    PeerWireMessage, ProviderData, RepoIdentity, RoutedPeerMessage, TopologyRoute, VectorClock,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::transport::{PeerSender, PeerTransport};

/// Generate a synthetic path for a remote-only repo.
///
/// Remote-only repos have no local filesystem path. This function produces
/// a deterministic `PathBuf` that serves as a stable key for tab identity
/// and repo tracking, e.g. `<remote>/desktop/home/dev/repo`.
pub fn synthetic_repo_path(host: &HostName, repo_path: &Path) -> PathBuf {
    // Strip leading `/` from absolute paths to avoid double-slash in the
    // resulting string (e.g. `<remote>/desktop//home/...`).
    let stripped = repo_path.strip_prefix("/").unwrap_or(repo_path);
    PathBuf::from(format!("<remote>/{}/{}", host, stripped.display()))
}

/// Result of handling an inbound PeerDataMessage.
#[derive(Debug, PartialEq, Eq)]
pub enum HandleResult {
    /// Data was updated for this repo — caller should trigger re-merge.
    Updated(RepoIdentity),
    /// The sender is requesting a resync — caller should send a snapshot back.
    ResyncRequested { request_id: u64, requester_host: HostName, reply_via: HostName, repo: RepoIdentity, since_seq: u64 },
    /// Peer intentionally retired this connection; reconnect should be suppressed briefly.
    ReconnectSuppressed { peer: HostName },
    /// A delta was received but cannot be applied (seq gap or not yet implemented).
    /// Caller should request a full resync from the origin.
    NeedsResync { from: HostName, repo: RepoIdentity },
    /// A routed command targeted this daemon and should be executed locally.
    CommandRequested { request_id: u64, requester_host: HostName, reply_via: HostName, command: Command },
    /// A routed command cancel request targeted this daemon.
    CommandCancelRequested { cancel_id: u64, requester_host: HostName, reply_via: HostName, command_request_id: u64 },
    /// A routed command lifecycle event reached the original requester.
    CommandEventReceived { request_id: u64, responder_host: HostName, event: CommandPeerEvent },
    /// A routed command completed and the final result reached the requester.
    CommandResponseReceived { request_id: u64, responder_host: HostName, result: CommandResult },
    /// A routed command cancel response reached the original requester.
    CommandCancelResponseReceived { cancel_id: u64, responder_host: HostName, error: Option<String> },
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
    pub expected_peer: Option<HostName>,
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
    pub connection_peer: HostName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteHop {
    pub next_hop: HostName,
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
    pub requester_host: HostName,
    pub target_host: HostName,
    pub repo_identity: RepoIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommandReversePathKey {
    pub request_id: u64,
    pub requester_host: HostName,
    pub target_host: HostName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReversePathHop {
    pub next_hop: HostName,
    pub next_hop_generation: u64,
    pub learned_at: u64,
}

#[derive(Debug, Clone)]
pub struct PendingResyncRequest {
    pub deadline_at: Instant,
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
    SetProviders { identity: RepoIdentity, peers: Vec<(HostName, ProviderData)> },
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
    pub repo_path: PathBuf,
    pub seq: u64,
    pub via_peer: HostName,
    pub via_generation: u64,
    pub stale: bool,
}

/// Manages connections to remote peer hosts and stores their provider data.
///
/// The PeerManager does NOT own the InProcessDaemon. It returns information
/// about what changed so the caller (DaemonServer / wiring code) can trigger
/// re-merge on the daemon.
pub struct PeerManager {
    local_host: HostName,
    peers: HashMap<HostName, Box<dyn PeerTransport>>,
    senders: HashMap<HostName, Arc<dyn PeerSender>>,
    active_connections: HashMap<HostName, ActiveConnection>,
    displaced_senders: HashMap<(HostName, u64), Arc<dyn PeerSender>>,
    reconnect_suppressed_until: HashMap<HostName, Instant>,
    transport_peers: HashMap<ConfigLabel, HostName>,
    generations: HashMap<HostName, u64>,
    routes: HashMap<HostName, RouteState>,
    /// TODO: expire abandoned reverse-path entries when routed replies time out
    /// instead of only clearing them on reply delivery or disconnect.
    reverse_paths: HashMap<ReversePathKey, ReversePathHop>,
    command_reverse_paths: HashMap<CommandReversePathKey, ReversePathHop>,
    /// TODO: sweep overdue requests by deadline_at; today these are removed on
    /// reply, targeted disconnect, or process restart.
    pending_resync_requests: HashMap<ReversePathKey, PendingResyncRequest>,
    route_epoch: u64,
    request_id_counter: u64,
    peer_data: HashMap<HostName, HashMap<RepoIdentity, PerRepoPeerState>>,
    peer_host_summaries: HashMap<HostName, HostSummary>,
    /// RepoIdentity values that exist only on remote peers — no local repo
    /// matches. Each maps to the synthetic path used for tab identity.
    known_remote_repos: HashMap<RepoIdentity, PathBuf>,
    /// Last-seen vector clock per (origin_host, repo_identity) — used to
    /// detect and drop duplicate / already-seen messages.
    last_seen_clocks: HashMap<(HostName, RepoIdentity), VectorClock>,
}

impl PeerManager {
    const RESYNC_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
    const GOODBYE_RECONNECT_SUPPRESSION: Duration = Duration::from_secs(15);
    pub(crate) const DEFAULT_ROUTED_HOPS: u8 = 8;

    /// Create a new PeerManager with no peers.
    pub fn new(local_host: HostName) -> Self {
        Self {
            local_host,
            peers: HashMap::new(),
            senders: HashMap::new(),
            active_connections: HashMap::new(),
            displaced_senders: HashMap::new(),
            reconnect_suppressed_until: HashMap::new(),
            transport_peers: HashMap::new(),
            generations: HashMap::new(),
            routes: HashMap::new(),
            reverse_paths: HashMap::new(),
            pending_resync_requests: HashMap::new(),
            route_epoch: 0,
            request_id_counter: 0,
            peer_data: HashMap::new(),
            peer_host_summaries: HashMap::new(),
            known_remote_repos: HashMap::new(),
            command_reverse_paths: HashMap::new(),
            last_seen_clocks: HashMap::new(),
        }
    }

    /// Register a peer transport.
    pub fn add_peer(&mut self, name: HostName, transport: Box<dyn PeerTransport>) {
        info!(peer = %name, "registered peer transport");
        self.peers.insert(name, transport);
    }

    /// Register or replace a sender for a connected peer.
    pub fn register_sender(&mut self, name: HostName, sender: Arc<dyn PeerSender>) {
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

    pub fn note_pending_resync_request(&mut self, target_host: HostName, repo_identity: RepoIdentity) -> u64 {
        let request_id = self.next_request_id();
        self.pending_resync_requests.insert(
            ReversePathKey { request_id, requester_host: self.local_host.clone(), target_host, repo_identity },
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
            let origin = key.target_host.clone();
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

        affected_repos
    }

    pub fn current_generation(&self, name: &HostName) -> Option<u64> {
        self.active_connections.get(name).map(|active| active.generation)
    }

    /// Return the session ID for a connected peer, if known.
    pub fn peer_session_id(&self, host: &HostName) -> Option<uuid::Uuid> {
        self.active_connections.get(host).and_then(|active| active.session_id)
    }

    pub fn reconnect_suppressed_until(&mut self, name: &HostName) -> Option<Instant> {
        match self.reconnect_suppressed_until.get(name).copied() {
            Some(deadline) if deadline > Instant::now() => Some(deadline),
            Some(_) => {
                self.reconnect_suppressed_until.remove(name);
                None
            }
            None => None,
        }
    }

    fn generation_is_current(&self, name: &HostName, generation: u64) -> bool {
        generation != 0 && self.generations.get(name).copied() == Some(generation)
    }

    fn install_direct_route(&mut self, host: &HostName, generation: u64) {
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

    fn retain_unique_hops(hops: &mut Vec<RouteHop>, next_hop: &HostName) {
        hops.retain(|hop| hop.next_hop != *next_hop);
    }

    fn observe_route(&mut self, origin: &HostName, via_peer: &HostName, via_generation: u64) {
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

    fn promote_route_after_disconnect(&mut self, origin: &HostName) -> Option<RouteHop> {
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

    fn winning_direction(&self, host: &HostName) -> ConnectionDirection {
        if self.local_host.as_str() < host.as_str() {
            ConnectionDirection::Outbound
        } else {
            ConnectionDirection::Inbound
        }
    }

    fn candidate_matches_winning_direction(&self, host: &HostName, meta: &ConnectionMeta) -> bool {
        meta.direction == self.winning_direction(host)
    }

    fn should_accept_candidate(&self, host: &HostName, active: &ActiveConnection, candidate: &ConnectionMeta) -> bool {
        let active_matches = self.candidate_matches_winning_direction(host, &active.meta);
        let candidate_matches = self.candidate_matches_winning_direction(host, candidate);

        match (active_matches, candidate_matches) {
            (false, true) => true,
            (true, false) => false,
            _ => false,
        }
    }

    pub fn activate_connection(&mut self, host: HostName, sender: Arc<dyn PeerSender>, meta: ConnectionMeta) -> ActivationResult {
        self.activate_connection_with_session(host, sender, meta, None)
    }

    pub fn activate_connection_with_session(
        &mut self,
        host: HostName,
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

    fn store_snapshot_from(&mut self, via_peer: &HostName, via_generation: u64, msg: PeerDataMessage) -> HandleResult {
        let origin = msg.origin_host.clone();
        let repo = msg.repo_identity.clone();
        let repo_path = msg.repo_path.clone();

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
                    repo_path,
                    seq,
                    via_peer: via_peer.clone(),
                    via_generation,
                    stale: false,
                });

                self.observe_route(&origin, via_peer, via_generation);

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
                if msg.origin_host == self.local_host {
                    debug!(host = %msg.origin_host, "ignoring peer data from self");
                    return HandleResult::Ignored;
                }
                self.store_snapshot_from(&env.connection_peer, env.connection_generation, msg)
            }
            PeerWireMessage::HostSummary(mut summary) => {
                summary.host_name = env.connection_peer.clone();
                self.store_host_summary(summary);
                HandleResult::Ignored
            }
            PeerWireMessage::Routed(msg) => self.handle_routed(env.connection_peer, env.connection_generation, msg).await,
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

    async fn handle_routed(&mut self, connection_peer: HostName, connection_generation: u64, msg: RoutedPeerMessage) -> HandleResult {
        match msg {
            RoutedPeerMessage::RequestResync { request_id, requester_host, target_host, remaining_hops, repo_identity, since_seq } => {
                if remaining_hops == 0 {
                    return HandleResult::Ignored;
                }
                if target_host == self.local_host {
                    return HandleResult::ResyncRequested {
                        request_id,
                        requester_host,
                        reply_via: connection_peer,
                        repo: repo_identity,
                        since_seq,
                    };
                }

                let key = ReversePathKey {
                    request_id,
                    requester_host: requester_host.clone(),
                    target_host: target_host.clone(),
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
                    requester_host,
                    target_host: target_host.clone(),
                    remaining_hops: remaining_hops.saturating_sub(1),
                    repo_identity,
                    since_seq,
                };
                let _ = self.send_to(&target_host, PeerWireMessage::Routed(forwarded)).await;
                HandleResult::Ignored
            }
            RoutedPeerMessage::ResyncSnapshot {
                request_id,
                requester_host,
                responder_host,
                remaining_hops,
                repo_identity,
                repo_path,
                clock,
                seq,
                data,
            } => {
                let key = ReversePathKey {
                    request_id,
                    requester_host: requester_host.clone(),
                    target_host: responder_host.clone(),
                    repo_identity: repo_identity.clone(),
                };

                if requester_host == self.local_host {
                    if self.pending_resync_requests.remove(&key).is_none() {
                        return HandleResult::Ignored;
                    }
                    self.last_seen_clocks.remove(&(responder_host.clone(), repo_identity.clone()));
                    return self.store_snapshot_from(&connection_peer, connection_generation, PeerDataMessage {
                        origin_host: responder_host,
                        repo_identity,
                        repo_path,
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
                    requester_host,
                    responder_host,
                    remaining_hops: remaining_hops.saturating_sub(1),
                    repo_identity,
                    repo_path,
                    clock,
                    seq,
                    data,
                };
                if let Some(sender) = self.senders.get(&reverse_hop.next_hop) {
                    let _ = sender.send(PeerWireMessage::Routed(forwarded)).await;
                }
                self.reverse_paths.remove(&key);
                HandleResult::Ignored
            }
            RoutedPeerMessage::CommandRequest { request_id, requester_host, target_host, remaining_hops, command } => {
                if remaining_hops == 0 {
                    return HandleResult::Ignored;
                }
                if target_host == self.local_host {
                    return HandleResult::CommandRequested { request_id, requester_host, reply_via: connection_peer, command: *command };
                }

                let key = CommandReversePathKey { request_id, requester_host: requester_host.clone(), target_host: target_host.clone() };
                let learned_at = self.next_route_epoch();
                self.command_reverse_paths.insert(key, ReversePathHop {
                    next_hop: connection_peer,
                    next_hop_generation: connection_generation,
                    learned_at,
                });

                let forwarded = RoutedPeerMessage::CommandRequest {
                    request_id,
                    requester_host,
                    target_host: target_host.clone(),
                    remaining_hops: remaining_hops.saturating_sub(1),
                    command,
                };
                let _ = self.send_to(&target_host, PeerWireMessage::Routed(forwarded)).await;
                HandleResult::Ignored
            }
            RoutedPeerMessage::CommandCancelRequest { cancel_id, requester_host, target_host, remaining_hops, command_request_id } => {
                if remaining_hops == 0 {
                    return HandleResult::Ignored;
                }
                if target_host == self.local_host {
                    return HandleResult::CommandCancelRequested {
                        cancel_id,
                        requester_host,
                        reply_via: connection_peer,
                        command_request_id,
                    };
                }

                let key = CommandReversePathKey {
                    request_id: cancel_id,
                    requester_host: requester_host.clone(),
                    target_host: target_host.clone(),
                };
                let learned_at = self.next_route_epoch();
                self.command_reverse_paths.insert(key, ReversePathHop {
                    next_hop: connection_peer,
                    next_hop_generation: connection_generation,
                    learned_at,
                });

                let forwarded = RoutedPeerMessage::CommandCancelRequest {
                    cancel_id,
                    requester_host,
                    target_host: target_host.clone(),
                    remaining_hops: remaining_hops.saturating_sub(1),
                    command_request_id,
                };
                let _ = self.send_to(&target_host, PeerWireMessage::Routed(forwarded)).await;
                HandleResult::Ignored
            }
            RoutedPeerMessage::CommandEvent { request_id, requester_host, responder_host, remaining_hops, event } => {
                let key = CommandReversePathKey { request_id, requester_host: requester_host.clone(), target_host: responder_host.clone() };

                if requester_host == self.local_host {
                    return HandleResult::CommandEventReceived { request_id, responder_host, event: *event };
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
                    requester_host,
                    responder_host,
                    remaining_hops: remaining_hops.saturating_sub(1),
                    event,
                };
                if let Some(sender) = self.senders.get(&reverse_hop.next_hop) {
                    let _ = sender.send(PeerWireMessage::Routed(forwarded)).await;
                }
                HandleResult::Ignored
            }
            RoutedPeerMessage::CommandResponse { request_id, requester_host, responder_host, remaining_hops, result } => {
                let key = CommandReversePathKey { request_id, requester_host: requester_host.clone(), target_host: responder_host.clone() };

                if requester_host == self.local_host {
                    return HandleResult::CommandResponseReceived { request_id, responder_host, result: *result };
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
                    requester_host,
                    responder_host,
                    remaining_hops: remaining_hops.saturating_sub(1),
                    result,
                };
                if let Some(sender) = self.senders.get(&reverse_hop.next_hop) {
                    let _ = sender.send(PeerWireMessage::Routed(forwarded)).await;
                }
                self.command_reverse_paths.remove(&key);
                HandleResult::Ignored
            }
            RoutedPeerMessage::CommandCancelResponse { cancel_id, requester_host, responder_host, remaining_hops, error } => {
                let key = CommandReversePathKey {
                    request_id: cancel_id,
                    requester_host: requester_host.clone(),
                    target_host: responder_host.clone(),
                };

                if requester_host == self.local_host {
                    return HandleResult::CommandCancelResponseReceived { cancel_id, responder_host, error };
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
                    requester_host,
                    responder_host,
                    remaining_hops: remaining_hops.saturating_sub(1),
                    error,
                };
                if let Some(sender) = self.senders.get(&reverse_hop.next_hop) {
                    let _ = sender.send(PeerWireMessage::Routed(forwarded)).await;
                }
                self.command_reverse_paths.remove(&key);
                HandleResult::Ignored
            }
        }
    }

    /// Forward a message to all connected peers except the origin, self,
    /// and any host already present in the message's vector clock (which
    /// indicates that host has already seen or relayed the message).
    pub async fn relay(&self, origin: &HostName, msg: &PeerDataMessage) {
        // Stamp our own host into the clock before relaying
        let mut relayed_msg = msg.clone();
        relayed_msg.clock.tick(&self.local_host);

        for (name, sender) in &self.senders {
            if name == origin || name == &self.local_host {
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
    pub fn local_host(&self) -> &HostName {
        &self.local_host
    }

    /// Accessor for all stored peer data — used by the merge layer.
    pub fn get_peer_data(&self) -> &HashMap<HostName, HashMap<RepoIdentity, PerRepoPeerState>> {
        &self.peer_data
    }

    pub fn store_host_summary(&mut self, summary: HostSummary) {
        self.peer_host_summaries.insert(summary.host_name.clone(), summary);
    }

    pub fn get_peer_host_summaries(&self) -> &HashMap<HostName, HostSummary> {
        &self.peer_host_summaries
    }

    pub fn topology_routes(&self) -> Vec<TopologyRoute> {
        let mut routes: Vec<_> = self
            .routes
            .iter()
            .map(|(target, route)| TopologyRoute {
                target: target.clone(),
                next_hop: route.primary.next_hop.clone(),
                direct: route.primary.next_hop == *target,
                connected: self.route_hop_is_live(&route.primary),
                fallbacks: route.fallbacks.iter().filter(|hop| self.route_hop_is_live(hop)).map(|hop| hop.next_hop.clone()).collect(),
            })
            .collect();
        routes.sort_by(|a, b| a.target.cmp(&b.target));
        routes
    }

    /// Snapshot relay targets without performing any async sends.
    ///
    /// Returns a list of `(target, sender, stamped message)` tuples for peers
    /// that should receive the relayed message. The caller sends concurrently
    /// outside the PeerManager lock, eliminating head-of-line blocking.
    pub fn prepare_relay(&self, origin: &HostName, msg: &PeerDataMessage) -> Vec<(HostName, Arc<dyn PeerSender>, PeerDataMessage)> {
        let mut relayed_msg = msg.clone();
        relayed_msg.clock.tick(&self.local_host);

        self.senders
            .iter()
            .filter(|(name, _)| *name != origin && *name != &self.local_host && msg.clock.get(name) == 0)
            .map(|(name, sender)| (name.clone(), Arc::clone(sender), relayed_msg.clone()))
            .collect()
    }

    /// Connect all registered peer transports and return inbound receivers.
    ///
    /// For each successfully connected peer, calls `subscribe()` to obtain the
    /// inbound message receiver. The caller should spawn forwarding tasks that
    /// feed these receivers into the shared `peer_data_tx` channel.
    pub async fn connect_all(&mut self) -> Vec<(HostName, u64, mpsc::Receiver<PeerWireMessage>)> {
        let names: Vec<HostName> = self.peers.keys().cloned().collect();
        let mut receivers = Vec::new();
        for name in names {
            let connect_result = if let Some(transport) = self.peers.get_mut(&name) {
                match transport.connect().await {
                    Ok(()) => {
                        let sender = transport.sender();
                        let subscribe_result = transport.subscribe().await;
                        let remote_session_id = transport.remote_session_id();
                        Ok((sender, subscribe_result, remote_session_id))
                    }
                    Err(e) => Err(e),
                }
            } else {
                continue;
            };

            match connect_result {
                Ok((sender, subscribe_result, remote_session_id)) => {
                    info!(peer = %name, "peer transport connected");
                    let mut generation = 0;
                    if let Some(sender) = sender {
                        let displaced = match self.activate_connection_with_session(
                            name.clone(),
                            sender,
                            ConnectionMeta {
                                direction: ConnectionDirection::Outbound,
                                config_label: None,
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
                                if let Some(transport) = self.peers.get_mut(&name) {
                                    let _ = transport.disconnect().await;
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
                        Ok(rx) => receivers.push((name.clone(), generation, rx)),
                        Err(e) => {
                            warn!(peer = %name, err = %e, "failed to subscribe to peer");
                        }
                    }
                }
                Err(e) => {
                    warn!(peer = %name, err = %e, "failed to connect peer transport");
                }
            }
        }
        receivers
    }

    /// Disconnect all registered peer transports.
    pub async fn disconnect_all(&mut self) {
        let names: Vec<HostName> = self.peers.keys().cloned().collect();
        for name in names {
            if let Some(transport) = self.peers.get_mut(&name) {
                match transport.disconnect().await {
                    Ok(()) => {
                        self.senders.remove(&name);
                        info!(peer = %name, "peer transport disconnected");
                    }
                    Err(e) => {
                        warn!(peer = %name, err = %e, "failed to disconnect peer transport");
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

    /// Iterate over all registered peer transports.
    pub fn peers(&self) -> &HashMap<HostName, Box<dyn PeerTransport>> {
        &self.peers
    }

    pub fn configured_peer_names(&self) -> Vec<HostName> {
        self.peers.keys().cloned().collect()
    }

    /// Return the currently addressable peers that have active senders.
    pub fn active_peers(&self) -> Vec<HostName> {
        self.senders.keys().cloned().collect()
    }

    pub fn active_peer_senders(&self) -> Vec<(HostName, Arc<dyn PeerSender>)> {
        self.senders.iter().map(|(name, sender)| (name.clone(), Arc::clone(sender))).collect()
    }

    /// Returns the sender for a peer only if the given generation matches
    /// the peer's current generation. Used by targeted sends to avoid
    /// sending to a connection that has been superseded.
    pub fn get_sender_if_current(&self, peer: &HostName, generation: u64) -> Option<Arc<dyn PeerSender>> {
        if !self.generation_is_current(peer, generation) {
            return None;
        }
        self.senders.get(peer).cloned()
    }

    pub fn resolve_sender(&self, name: &HostName) -> Result<Arc<dyn PeerSender>, String> {
        if let Some(sender) = self.senders.get(name) {
            return Ok(Arc::clone(sender));
        }

        let route = self.routes.get(name).ok_or_else(|| format!("unknown peer: {name}"))?;
        self.senders.get(&route.primary.next_hop).cloned().ok_or_else(|| format!("missing next hop sender: {}", route.primary.next_hop))
    }

    pub fn take_displaced_sender(&mut self, name: &HostName, generation: u64) -> Option<Arc<dyn PeerSender>> {
        self.displaced_senders.remove(&(name.clone(), generation))
    }

    /// Remove all stored data for a peer (e.g. on disconnect).
    ///
    /// Returns the list of RepoIdentity values that were affected, so the
    /// caller can rebuild the daemon's peer overlay for those repos.
    pub fn remove_peer_data(&mut self, name: &HostName) -> Vec<RepoIdentity> {
        let affected: Vec<RepoIdentity> = self.peer_data.get(name).map(|repos| repos.keys().cloned().collect()).unwrap_or_default();
        self.peer_data.remove(name);
        self.peer_host_summaries.remove(name);
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
    pub async fn send_to(&self, name: &HostName, msg: PeerWireMessage) -> Result<(), String> {
        let sender = self.resolve_sender(name)?;
        sender.send(msg).await
    }

    /// Reconnect a specific peer: disconnect, then connect + subscribe.
    ///
    /// Returns the new inbound receiver on success.
    pub async fn reconnect_peer(&mut self, name: &HostName) -> Result<(u64, mpsc::Receiver<PeerWireMessage>), String> {
        if let Some(deadline) = self.reconnect_suppressed_until(name) {
            return Err(format!("reconnect suppressed until {:?}", deadline));
        }

        let (sender, rx, remote_session_id) = {
            let transport = self.peers.get_mut(name).ok_or_else(|| format!("unknown peer: {name}"))?;

            // Best-effort disconnect before reconnecting
            let _ = transport.disconnect().await;

            transport.connect().await?;
            let sender = transport.sender();
            let rx = transport.subscribe().await?;
            let remote_session_id = transport.remote_session_id();
            (sender, rx, remote_session_id)
        };

        let mut generation = 0;
        if let Some(sender) = sender {
            let displaced = match self.activate_connection_with_session(
                name.clone(),
                sender,
                ConnectionMeta {
                    direction: ConnectionDirection::Outbound,
                    config_label: None,
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
                    if let Some(transport) = self.peers.get_mut(name) {
                        let _ = transport.disconnect().await;
                    }
                    return Err(format!("connection for {name} lost duplicate arbitration"));
                }
            };
            if let Some(displaced_generation) = displaced {
                if let Some(displaced_sender) = self.take_displaced_sender(name, displaced_generation) {
                    let _ = displaced_sender.retire(GoodbyeReason::Superseded).await;
                }
            }
        }

        Ok((generation, rx))
    }

    /// Clear all stored peer data originating from a specific host.
    ///
    /// Used when a remote daemon restart is detected (session_id changed).
    /// Unlike `disconnect_peer`, this does NOT tear down the connection.
    pub fn clear_peer_data_for_restart(&mut self, origin: &HostName) -> Vec<RepoIdentity> {
        let Some(repos) = self.peer_data.remove(origin) else {
            // Restart cleanup still owns host-summary eviction even when no repo snapshots were cached.
            self.peer_host_summaries.remove(origin);
            return Vec::new();
        };
        let affected: Vec<RepoIdentity> = repos.keys().cloned().collect();
        self.peer_host_summaries.remove(origin);
        self.last_seen_clocks.retain(|(host, _), _| host != origin);
        info!(peer = %origin, repo_count = affected.len(), "cleared stale peer data after restart");
        affected
    }

    pub fn disconnect_peer(&mut self, name: &HostName, generation: u64) -> DisconnectPlan {
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
        self.reverse_paths.retain(|_, hop| hop.next_hop != *name);
        self.command_reverse_paths.retain(|_, hop| hop.next_hop != *name);
        self.pending_resync_requests.retain(|key, _| key.target_host != *name);
        self.peer_host_summaries.remove(name);

        let mut affected_repos = Vec::new();
        let mut resync_requests = Vec::new();
        let origins: Vec<HostName> = self.peer_data.keys().cloned().collect();

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
                        requester_host: self.local_host.clone(),
                        target_host: origin.clone(),
                        repo_identity: repo_id.clone(),
                    };
                    self.pending_resync_requests
                        .insert(key, PendingResyncRequest { deadline_at: Instant::now() + Self::RESYNC_REQUEST_TIMEOUT });
                    resync_requests.push(RoutedPeerMessage::RequestResync {
                        request_id,
                        requester_host: self.local_host.clone(),
                        target_host: origin.clone(),
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
        let mut overlay_updates = Vec::new();
        for repo_id in &affected_repos {
            if self.has_peer_data_for(repo_id) {
                // Repo still has data from other peers — collect remaining peer data
                let peers: Vec<(HostName, ProviderData)> = self
                    .peer_data
                    .iter()
                    .filter_map(|(host, repos)| repos.get(repo_id).map(|state| (host.clone(), state.provider_data.clone())))
                    .collect();
                overlay_updates.push(OverlayUpdate::SetProviders { identity: repo_id.clone(), peers });
            } else if let Some(synthetic_path) = self.unregister_remote_repo(repo_id) {
                // Remote-only, no peers remain — remove the virtual tab
                overlay_updates.push(OverlayUpdate::RemoveRepo { identity: repo_id.clone(), path: synthetic_path });
            }
        }

        DisconnectPlan { was_active: true, affected_repos, resync_requests, overlay_updates }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tokio::sync::mpsc;

    use super::{super::transport::PeerConnectionStatus, *};
    use crate::peer::test_support::{ensure_test_connection_generation, handle_test_peer_data};

    struct MockPeerSender {
        sent: Arc<Mutex<Vec<PeerWireMessage>>>,
    }

    #[async_trait]
    impl PeerSender for MockPeerSender {
        async fn send(&self, msg: PeerWireMessage) -> Result<(), String> {
            self.sent.lock().expect("lock poisoned").push(msg);
            Ok(())
        }

        async fn retire(&self, reason: GoodbyeReason) -> Result<(), String> {
            self.sent.lock().expect("lock poisoned").push(PeerWireMessage::Goodbye { reason });
            Ok(())
        }
    }

    /// Mock transport that tracks connection status and optionally exposes a sender.
    struct MockTransport {
        status: PeerConnectionStatus,
        sender: Option<Arc<dyn PeerSender>>,
    }

    impl MockTransport {
        fn new() -> Self {
            Self { status: PeerConnectionStatus::Connected, sender: None }
        }

        fn with_sender() -> (Self, Arc<Mutex<Vec<PeerWireMessage>>>) {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&sent) });
            let transport = Self { status: PeerConnectionStatus::Connected, sender: Some(sender) };
            (transport, sent)
        }
    }

    #[async_trait]
    impl PeerTransport for MockTransport {
        async fn connect(&mut self) -> Result<(), String> {
            self.status = PeerConnectionStatus::Connected;
            Ok(())
        }

        async fn disconnect(&mut self) -> Result<(), String> {
            self.status = PeerConnectionStatus::Disconnected;
            Ok(())
        }

        fn status(&self) -> PeerConnectionStatus {
            self.status.clone()
        }

        async fn subscribe(&mut self) -> Result<mpsc::Receiver<PeerWireMessage>, String> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        }

        fn sender(&self) -> Option<Arc<dyn PeerSender>> {
            self.sender.clone()
        }
    }

    fn test_repo() -> RepoIdentity {
        RepoIdentity { authority: "github.com".into(), path: "owner/repo".into() }
    }

    fn snapshot_msg(origin: &str, seq: u64) -> PeerDataMessage {
        let mut clock = VectorClock::default();
        for _ in 0..seq {
            clock.tick(&HostName::new(origin));
        }
        PeerDataMessage {
            origin_host: HostName::new(origin),
            repo_identity: test_repo(),
            repo_path: PathBuf::from("/home/dev/repo"),
            clock,
            kind: PeerDataKind::Snapshot { data: Box::new(ProviderData::default()), seq },
        }
    }

    fn sample_host_summary_for(name: &str) -> flotilla_protocol::HostSummary {
        flotilla_protocol::HostSummary {
            host_name: HostName::new(name),
            system: flotilla_protocol::SystemInfo {
                home_dir: Some(PathBuf::from("/home/dev")),
                os: Some("linux".into()),
                arch: Some("x86_64".into()),
                cpu_count: Some(8),
                memory_total_mb: Some(16384),
                environment: flotilla_protocol::HostEnvironment::Unknown,
            },
            inventory: flotilla_protocol::ToolInventory::default(),
            providers: vec![],
        }
    }

    fn accepted_generation(result: ActivationResult) -> u64 {
        match result {
            ActivationResult::Accepted { generation, .. } => generation,
            ActivationResult::Rejected { reason } => {
                panic!("expected accepted connection, got rejection: {:?}", reason)
            }
        }
    }

    #[tokio::test]
    async fn handle_snapshot_stores_data() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let msg = snapshot_msg("remote", 1);

        let result = handle_test_peer_data(&mut mgr, msg, || {
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
        })
        .await;
        assert_eq!(result, HandleResult::Updated(test_repo()));

        let peer_data = mgr.get_peer_data();
        let remote_host = HostName::new("remote");
        assert!(peer_data.contains_key(&remote_host));
        let repo_state = &peer_data[&remote_host][&test_repo()];
        assert_eq!(repo_state.seq, 1);
        assert_eq!(repo_state.repo_path, PathBuf::from("/home/dev/repo"));
    }

    #[tokio::test]
    async fn handle_snapshot_updates_existing_data() {
        let mut mgr = PeerManager::new(HostName::new("local"));

        // First snapshot
        let msg1 = snapshot_msg("remote", 1);
        handle_test_peer_data(&mut mgr, msg1, || {
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
        })
        .await;

        // Second snapshot with higher seq
        let msg2 = snapshot_msg("remote", 5);
        let result = handle_test_peer_data(&mut mgr, msg2, || {
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
        })
        .await;
        assert_eq!(result, HandleResult::Updated(test_repo()));

        let peer_data = mgr.get_peer_data();
        let repo_state = &peer_data[&HostName::new("remote")][&test_repo()];
        assert_eq!(repo_state.seq, 5);
    }

    #[tokio::test]
    async fn legacy_direct_request_resync_is_ignored() {
        let mut mgr = PeerManager::new(HostName::new("local"));

        let msg = PeerDataMessage {
            origin_host: HostName::new("remote"),
            repo_identity: test_repo(),
            repo_path: PathBuf::from("/home/dev/repo"),
            clock: VectorClock::default(),
            kind: PeerDataKind::RequestResync { since_seq: 3 },
        };

        let result = handle_test_peer_data(&mut mgr, msg, || {
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
        })
        .await;
        assert_eq!(result, HandleResult::Ignored);
    }

    #[tokio::test]
    async fn handle_delta_returns_needs_resync() {
        use flotilla_protocol::{
            delta::{Branch, BranchStatus, EntryOp},
            Change,
        };

        let mut mgr = PeerManager::new(HostName::new("local"));

        let msg = PeerDataMessage {
            origin_host: HostName::new("remote"),
            repo_identity: test_repo(),
            repo_path: PathBuf::from("/home/dev/repo"),
            clock: VectorClock::default(),
            kind: PeerDataKind::Delta {
                changes: vec![Change::Branch { key: "feat-x".into(), op: EntryOp::Added(Branch { status: BranchStatus::Remote }) }],
                seq: 2,
                prev_seq: 1,
            },
        };

        let result = handle_test_peer_data(&mut mgr, msg, || {
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
        })
        .await;
        assert_eq!(result, HandleResult::NeedsResync { from: HostName::new("remote"), repo: test_repo() });
    }

    #[tokio::test]
    async fn handle_ignores_messages_from_self() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let msg = snapshot_msg("local", 1);

        let result = handle_test_peer_data(&mut mgr, msg, || {
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
        })
        .await;
        assert_eq!(result, HandleResult::Ignored);
        assert!(mgr.get_peer_data().is_empty());
    }

    #[tokio::test]
    async fn relay_sends_to_all_except_origin() {
        let mut mgr = PeerManager::new(HostName::new("local"));

        let (transport_a, sent_a) = MockTransport::with_sender();
        let (transport_b, sent_b) = MockTransport::with_sender();
        let (transport_c, sent_c) = MockTransport::with_sender();
        let sender_a = transport_a.sender().expect("sender");
        let sender_b = transport_b.sender().expect("sender");
        let sender_c = transport_c.sender().expect("sender");

        mgr.add_peer(HostName::new("peer-a"), Box::new(transport_a));
        mgr.add_peer(HostName::new("peer-b"), Box::new(transport_b));
        mgr.add_peer(HostName::new("peer-c"), Box::new(transport_c));
        mgr.register_sender(HostName::new("peer-a"), sender_a);
        mgr.register_sender(HostName::new("peer-b"), sender_b);
        mgr.register_sender(HostName::new("peer-c"), sender_c);

        let msg = snapshot_msg("peer-a", 1);
        mgr.relay(&HostName::new("peer-a"), &msg).await;

        // peer-a is origin, so it should NOT receive the relay
        assert!(sent_a.lock().expect("lock").is_empty());
        // peer-b and peer-c should each get exactly one message
        assert_eq!(sent_b.lock().expect("lock").len(), 1);
        assert_eq!(sent_c.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn relay_does_not_send_to_self() {
        let mut mgr = PeerManager::new(HostName::new("local"));

        let (transport, sent) = MockTransport::with_sender();
        let sender = transport.sender().expect("sender");
        mgr.add_peer(HostName::new("local"), Box::new(transport));
        mgr.register_sender(HostName::new("local"), sender);

        let msg = snapshot_msg("remote", 1);
        mgr.relay(&HostName::new("remote"), &msg).await;

        // Should not send to self even if registered as a peer
        assert!(sent.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn relay_skips_peers_already_in_clock() {
        // Star topology: leader has peers [F1, F2].
        // F1 sends a message that leader relays to F2 (stamping leader into clock).
        // If F2 then tried to relay, it should NOT send back to leader
        // because leader is already in the clock.
        let mut mgr = PeerManager::new(HostName::new("F2"));

        let (transport_leader, sent_leader) = MockTransport::with_sender();
        let sender_leader = transport_leader.sender().expect("sender");
        mgr.add_peer(HostName::new("leader"), Box::new(transport_leader));
        mgr.register_sender(HostName::new("leader"), sender_leader);

        // Simulate a message that was relayed through leader:
        // origin=F1, clock={F1:1, leader:1}
        let mut clock = VectorClock::default();
        clock.tick(&HostName::new("F1"));
        clock.tick(&HostName::new("leader"));
        let msg = PeerDataMessage {
            origin_host: HostName::new("F1"),
            repo_identity: test_repo(),
            repo_path: PathBuf::from("/home/dev/repo"),
            clock,
            kind: PeerDataKind::Snapshot { data: Box::new(ProviderData::default()), seq: 1 },
        };

        mgr.relay(&HostName::new("F1"), &msg).await;

        // Leader is already in the clock, so relay should skip it
        assert!(sent_leader.lock().expect("lock").is_empty(), "should not relay back to a peer already in the clock");
    }

    #[tokio::test]
    async fn get_peer_data_returns_stored_data() {
        let mut mgr = PeerManager::new(HostName::new("local"));

        // Initially empty
        assert!(mgr.get_peer_data().is_empty());

        // After storing data from two hosts
        handle_test_peer_data(&mut mgr, snapshot_msg("desktop", 1), || {
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
        })
        .await;
        handle_test_peer_data(&mut mgr, snapshot_msg("server", 2), || {
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
        })
        .await;

        let data = mgr.get_peer_data();
        assert_eq!(data.len(), 2);
        assert!(data.contains_key(&HostName::new("desktop")));
        assert!(data.contains_key(&HostName::new("server")));
    }

    #[tokio::test]
    async fn host_summary_handle_inbound_stores_for_connection_peer() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let connection_peer = HostName::new("remote");
        let generation = ensure_test_connection_generation(&mut mgr, &connection_peer, || {
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
        });

        let result = mgr
            .handle_inbound(InboundPeerEnvelope {
                msg: PeerWireMessage::HostSummary(sample_host_summary_for("spoofed-name")),
                connection_generation: generation,
                connection_peer: connection_peer.clone(),
            })
            .await;

        assert_eq!(result, HandleResult::Ignored);
        let stored = mgr.get_peer_host_summaries().get(&connection_peer).expect("stored host summary");
        assert_eq!(stored.host_name, connection_peer);
    }

    #[test]
    fn remove_peer_data_clears_host_summary() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        mgr.store_host_summary(sample_host_summary_for("remote"));

        mgr.remove_peer_data(&HostName::new("remote"));

        assert!(mgr.get_peer_host_summaries().is_empty());
    }

    #[test]
    fn clear_peer_data_for_restart_clears_host_summary() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        mgr.store_host_summary(sample_host_summary_for("remote"));

        mgr.clear_peer_data_for_restart(&HostName::new("remote"));

        assert!(mgr.get_peer_host_summaries().is_empty());
    }

    #[tokio::test]
    async fn connect_all_connects_peers() {
        let mut mgr = PeerManager::new(HostName::new("local"));

        let transport = MockTransport::new();
        // Start disconnected
        let mut transport = transport;
        transport.status = PeerConnectionStatus::Disconnected;

        mgr.add_peer(HostName::new("peer"), Box::new(transport));
        mgr.connect_all().await;

        // After connect_all, the mock transport's connect() sets status to Connected
        let peer_transport = mgr.peers.get(&HostName::new("peer")).expect("peer exists");
        assert_eq!(peer_transport.status(), PeerConnectionStatus::Connected);
    }

    #[tokio::test]
    async fn disconnect_all_disconnects_peers() {
        let mut mgr = PeerManager::new(HostName::new("local"));

        let transport = MockTransport::new();
        mgr.add_peer(HostName::new("peer"), Box::new(transport));
        mgr.disconnect_all().await;

        let peer_transport = mgr.peers.get(&HostName::new("peer")).expect("peer exists");
        assert_eq!(peer_transport.status(), PeerConnectionStatus::Disconnected);
    }

    #[test]
    fn synthetic_repo_path_format() {
        let host = HostName::new("desktop");
        let repo_path = std::path::Path::new("/home/dev/repo");
        let path = super::synthetic_repo_path(&host, repo_path);
        assert_eq!(path, PathBuf::from("<remote>/desktop/home/dev/repo"));
    }

    #[test]
    fn synthetic_repo_path_different_hosts_produce_different_paths() {
        let repo_path = std::path::Path::new("/home/dev/repo");
        let path_a = super::synthetic_repo_path(&HostName::new("host-a"), repo_path);
        let path_b = super::synthetic_repo_path(&HostName::new("host-b"), repo_path);
        assert_ne!(path_a, path_b);
    }

    #[test]
    fn register_and_query_remote_repos() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let repo = test_repo();
        let synthetic = PathBuf::from("<remote>/desktop/home/dev/repo");

        assert!(!mgr.is_remote_repo(&repo));
        assert!(mgr.known_remote_repos().is_empty());

        mgr.register_remote_repo(repo.clone(), synthetic.clone());

        assert!(mgr.is_remote_repo(&repo));
        assert_eq!(mgr.known_remote_repos().len(), 1);
        assert_eq!(mgr.known_remote_repos()[&repo], synthetic);
    }

    #[tokio::test]
    async fn send_to_reaches_registered_sender() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&sent) });
        mgr.register_sender(HostName::new("peer"), sender);

        mgr.send_to(&HostName::new("peer"), PeerWireMessage::Data(snapshot_msg("local", 1))).await.expect("send succeeds");

        assert_eq!(sent.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn activate_connection_rejects_same_direction_duplicate_sender() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let first_sent = Arc::new(Mutex::new(Vec::new()));
        let second_sent = Arc::new(Mutex::new(Vec::new()));
        let first_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&first_sent) });
        let second_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&second_sent) });

        let gen1 = accepted_generation(mgr.activate_connection(HostName::new("peer"), first_sender, ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));
        let second = mgr.activate_connection(HostName::new("peer"), second_sender, ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        });

        assert_eq!(gen1, 1);
        assert_eq!(second, ActivationResult::Rejected { reason: GoodbyeReason::Superseded });
        mgr.send_to(&HostName::new("peer"), PeerWireMessage::Data(snapshot_msg("local", 1))).await.expect("send succeeds");

        assert_eq!(first_sent.lock().expect("lock").len(), 1);
        assert!(second_sent.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn configured_outbound_beats_unsolicited_inbound() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let outbound_sent = Arc::new(Mutex::new(Vec::new()));
        let inbound_sent = Arc::new(Mutex::new(Vec::new()));
        let outbound_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&outbound_sent) });
        let inbound_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&inbound_sent) });

        let _ = accepted_generation(mgr.activate_connection(HostName::new("peer"), outbound_sender, ConnectionMeta {
            direction: ConnectionDirection::Outbound,
            config_label: Some(ConfigLabel("peer".into())),
            expected_peer: Some(HostName::new("peer")),
            config_backed: true,
        }));
        let duplicate = mgr.activate_connection(HostName::new("peer"), inbound_sender, ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        });
        assert_eq!(duplicate, ActivationResult::Rejected { reason: GoodbyeReason::Superseded });

        mgr.send_to(&HostName::new("peer"), PeerWireMessage::Data(snapshot_msg("local", 1))).await.expect("send succeeds");

        assert_eq!(outbound_sent.lock().expect("lock").len(), 1);
        assert!(inbound_sent.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn displaced_connection_can_be_retired_after_replacement() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let first_sent = Arc::new(Mutex::new(Vec::new()));
        let second_sent = Arc::new(Mutex::new(Vec::new()));
        let first_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&first_sent) });
        let second_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&second_sent) });

        let first_generation = accepted_generation(mgr.activate_connection(HostName::new("peer"), first_sender, ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));
        let replacement = mgr.activate_connection(HostName::new("peer"), second_sender, ConnectionMeta {
            direction: ConnectionDirection::Outbound,
            config_label: Some(ConfigLabel("peer".into())),
            expected_peer: Some(HostName::new("peer")),
            config_backed: true,
        });

        let displaced_generation = match replacement {
            ActivationResult::Accepted { generation, displaced: Some(displaced) } => {
                assert_eq!(generation, 2);
                displaced
            }
            other => panic!("expected accepted replacement, got {other:?}"),
        };
        assert_eq!(displaced_generation, first_generation);

        let displaced =
            mgr.take_displaced_sender(&HostName::new("peer"), displaced_generation).expect("displaced sender should be tracked");
        displaced.retire(GoodbyeReason::Superseded).await.expect("retire displaced sender");

        let sent = first_sent.lock().expect("lock");
        assert_eq!(sent.len(), 1);
        match &sent[0] {
            PeerWireMessage::Goodbye { reason: GoodbyeReason::Superseded } => {}
            other => panic!("expected superseded goodbye, got {other:?}"),
        }
        assert!(second_sent.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn stale_generation_inbound_message_is_dropped() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) });
        let generation = accepted_generation(mgr.activate_connection(HostName::new("peer"), sender, ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));
        assert_eq!(generation, 1);
        let replacement_generation = accepted_generation(mgr.activate_connection(
            HostName::new("peer"),
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }),
            ConnectionMeta {
                direction: ConnectionDirection::Outbound,
                config_label: None,
                expected_peer: Some(HostName::new("peer")),
                config_backed: true,
            },
        ));
        assert_eq!(replacement_generation, 2);

        let result = mgr
            .handle_inbound(InboundPeerEnvelope {
                msg: PeerWireMessage::Data(snapshot_msg("peer", 1)),
                connection_generation: generation,
                connection_peer: HostName::new("peer"),
            })
            .await;

        assert_eq!(result, HandleResult::Ignored);
        assert!(mgr.get_peer_data().is_empty());
    }

    #[tokio::test]
    async fn send_to_uses_route_primary_when_no_direct_sender() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let via_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&sent) });
        mgr.register_sender(HostName::new("relay"), via_sender);
        mgr.generations.insert(HostName::new("relay"), 1);
        mgr.routes.insert(HostName::new("target"), RouteState {
            primary: RouteHop { next_hop: HostName::new("relay"), next_hop_generation: 1, learned_epoch: 1 },
            fallbacks: Vec::new(),
            candidates: Vec::new(),
        });

        mgr.send_to(
            &HostName::new("target"),
            PeerWireMessage::Routed(RoutedPeerMessage::RequestResync {
                request_id: 1,
                requester_host: HostName::new("local"),
                target_host: HostName::new("target"),
                remaining_hops: 3,
                repo_identity: test_repo(),
                since_seq: 0,
            }),
        )
        .await
        .expect("send succeeds");

        assert_eq!(sent.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn send_to_returns_error_when_no_direct_sender_or_route() {
        let mgr = PeerManager::new(HostName::new("local"));
        let err = mgr
            .send_to(&HostName::new("missing"), PeerWireMessage::Data(snapshot_msg("local", 1)))
            .await
            .expect_err("missing route should error");
        assert!(err.contains("unknown peer"));
    }

    #[tokio::test]
    async fn configured_peer_names_include_all_configured_peers() {
        let mut mgr = PeerManager::new(HostName::new("m"));
        mgr.add_peer(HostName::new("z"), Box::new(MockTransport::new()));
        mgr.add_peer(HostName::new("a"), Box::new(MockTransport::new()));

        let mut configured = mgr.configured_peer_names();
        configured.sort();

        assert_eq!(configured, vec![HostName::new("a"), HostName::new("z")]);
    }

    #[tokio::test]
    async fn reconnect_peer_allows_configured_peer_regardless_of_host_order() {
        let mut mgr = PeerManager::new(HostName::new("z"));
        mgr.add_peer(HostName::new("a"), Box::new(MockTransport::new()));

        let (generation, _rx) = mgr.reconnect_peer(&HostName::new("a")).await.expect("reconnect should succeed for configured peer");

        assert_eq!(generation, 0);
    }

    #[tokio::test]
    async fn reconnect_peer_retires_displaced_connection() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let displaced_sent = Arc::new(Mutex::new(Vec::new()));
        let displaced_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&displaced_sent) });

        let _ = accepted_generation(mgr.activate_connection(HostName::new("peer"), displaced_sender, ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));

        let (transport, _new_sent) = MockTransport::with_sender();
        mgr.add_peer(HostName::new("peer"), Box::new(transport));

        let _ = mgr.reconnect_peer(&HostName::new("peer")).await.expect("reconnect should succeed");

        let sent = displaced_sent.lock().expect("lock");
        assert_eq!(sent.len(), 1);
        match &sent[0] {
            PeerWireMessage::Goodbye { reason: GoodbyeReason::Superseded } => {}
            other => panic!("expected superseded goodbye, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn late_resync_snapshot_is_dropped_without_pending_request() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) });
        let generation = accepted_generation(mgr.activate_connection(HostName::new("relay"), sender, ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));

        let result = mgr
            .handle_inbound(InboundPeerEnvelope {
                msg: PeerWireMessage::Routed(RoutedPeerMessage::ResyncSnapshot {
                    request_id: 1,
                    requester_host: HostName::new("local"),
                    responder_host: HostName::new("target"),
                    remaining_hops: 3,
                    repo_identity: test_repo(),
                    repo_path: PathBuf::from("/home/dev/repo"),
                    clock: VectorClock::default(),
                    seq: 1,
                    data: Box::new(ProviderData::default()),
                }),
                connection_generation: generation,
                connection_peer: HostName::new("relay"),
            })
            .await;

        assert_eq!(result, HandleResult::Ignored);
        assert!(mgr.get_peer_data().is_empty());
    }

    #[tokio::test]
    async fn goodbye_superseded_suppresses_reconnect_for_peer() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) });
        let generation = accepted_generation(mgr.activate_connection(HostName::new("peer"), sender, ConnectionMeta {
            direction: ConnectionDirection::Outbound,
            config_label: Some(ConfigLabel("peer".into())),
            expected_peer: Some(HostName::new("peer")),
            config_backed: true,
        }));
        mgr.add_peer(HostName::new("peer"), Box::new(MockTransport::new()));

        let result = mgr
            .handle_inbound(InboundPeerEnvelope {
                msg: PeerWireMessage::Goodbye { reason: GoodbyeReason::Superseded },
                connection_generation: generation,
                connection_peer: HostName::new("peer"),
            })
            .await;

        assert_eq!(result, HandleResult::ReconnectSuppressed { peer: HostName::new("peer") });
        let err = mgr.reconnect_peer(&HostName::new("peer")).await.expect_err("reconnect should be suppressed");
        assert!(err.contains("suppressed"));
    }

    #[tokio::test]
    async fn routed_request_resync_is_dropped_when_hop_budget_exhausted() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&sent) });
        let generation = accepted_generation(mgr.activate_connection(HostName::new("relay"), sender, ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));

        let result = mgr
            .handle_inbound(InboundPeerEnvelope {
                msg: PeerWireMessage::Routed(RoutedPeerMessage::RequestResync {
                    request_id: 1,
                    requester_host: HostName::new("requester"),
                    target_host: HostName::new("target"),
                    remaining_hops: 0,
                    repo_identity: test_repo(),
                    since_seq: 0,
                }),
                connection_generation: generation,
                connection_peer: HostName::new("relay"),
            })
            .await;

        assert_eq!(result, HandleResult::Ignored);
        assert!(sent.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn routed_request_resync_to_local_preserves_request_id() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) });
        let generation = accepted_generation(mgr.activate_connection(HostName::new("relay"), sender, ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));

        let result = mgr
            .handle_inbound(InboundPeerEnvelope {
                msg: PeerWireMessage::Routed(RoutedPeerMessage::RequestResync {
                    request_id: 41,
                    requester_host: HostName::new("requester"),
                    target_host: HostName::new("local"),
                    remaining_hops: 3,
                    repo_identity: test_repo(),
                    since_seq: 7,
                }),
                connection_generation: generation,
                connection_peer: HostName::new("relay"),
            })
            .await;

        assert_eq!(result, HandleResult::ResyncRequested {
            request_id: 41,
            requester_host: HostName::new("requester"),
            reply_via: HostName::new("relay"),
            repo: test_repo(),
            since_seq: 7,
        });
    }

    #[tokio::test]
    async fn disconnect_peer_keeps_snapshot_stale_when_fallback_exists() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let direct_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) });
        let relay_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) });
        let direct_generation = accepted_generation(mgr.activate_connection(HostName::new("target"), direct_sender, ConnectionMeta {
            direction: ConnectionDirection::Outbound,
            config_label: None,
            expected_peer: Some(HostName::new("target")),
            config_backed: true,
        }));
        let relay_generation = accepted_generation(mgr.activate_connection(HostName::new("relay"), relay_sender, ConnectionMeta {
            direction: ConnectionDirection::Outbound,
            config_label: None,
            expected_peer: Some(HostName::new("relay")),
            config_backed: true,
        }));
        let _ = mgr
            .handle_inbound(InboundPeerEnvelope {
                msg: PeerWireMessage::Data(snapshot_msg("target", 1)),
                connection_generation: direct_generation,
                connection_peer: HostName::new("target"),
            })
            .await;

        mgr.routes.get_mut(&HostName::new("target")).expect("route exists").fallbacks.push(RouteHop {
            next_hop: HostName::new("relay"),
            next_hop_generation: relay_generation,
            learned_epoch: 10,
        });

        let plan = mgr.disconnect_peer(&HostName::new("target"), direct_generation);

        assert_eq!(plan.affected_repos, vec![test_repo()]);
        assert_eq!(plan.resync_requests.len(), 1);
        let state = &mgr.get_peer_data()[&HostName::new("target")][&test_repo()];
        assert!(state.stale, "snapshot should be retained as stale");
        assert_eq!(mgr.routes[&HostName::new("target")].primary.next_hop, HostName::new("relay"));
    }

    #[tokio::test]
    async fn accepted_snapshot_refreshes_route_primary_to_live_hop() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let relay_a_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) });
        let relay_b_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) });
        let relay_a_generation = accepted_generation(mgr.activate_connection(HostName::new("relay-a"), relay_a_sender, ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));
        let relay_b_generation = accepted_generation(mgr.activate_connection(HostName::new("relay-b"), relay_b_sender, ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));

        let _ = mgr
            .handle_inbound(InboundPeerEnvelope {
                msg: PeerWireMessage::Data(snapshot_msg("target", 1)),
                connection_generation: relay_a_generation,
                connection_peer: HostName::new("relay-a"),
            })
            .await;
        let _ = mgr
            .handle_inbound(InboundPeerEnvelope {
                msg: PeerWireMessage::Data(snapshot_msg("target", 2)),
                connection_generation: relay_b_generation,
                connection_peer: HostName::new("relay-b"),
            })
            .await;

        assert_eq!(mgr.routes[&HostName::new("target")].primary.next_hop, HostName::new("relay-b"));
        assert_eq!(mgr.routes[&HostName::new("target")].fallbacks[0].next_hop, HostName::new("relay-a"));
    }

    #[tokio::test]
    async fn disconnect_peer_keeps_unrelated_pending_resync_requests() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let target_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) });
        let other_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) });

        let _ = accepted_generation(mgr.activate_connection(HostName::new("target"), target_sender, ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));
        let other_generation = accepted_generation(mgr.activate_connection(HostName::new("other"), other_sender, ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));

        let kept_request_id = mgr.note_pending_resync_request(HostName::new("target"), test_repo());
        let dropped_request_id = mgr.note_pending_resync_request(HostName::new("other"), test_repo());

        let _ = mgr.disconnect_peer(&HostName::new("other"), other_generation);

        let kept_key = ReversePathKey {
            request_id: kept_request_id,
            requester_host: HostName::new("local"),
            target_host: HostName::new("target"),
            repo_identity: test_repo(),
        };
        let dropped_key = ReversePathKey {
            request_id: dropped_request_id,
            requester_host: HostName::new("local"),
            target_host: HostName::new("other"),
            repo_identity: test_repo(),
        };

        assert!(mgr.pending_resync_requests.contains_key(&kept_key));
        assert!(!mgr.pending_resync_requests.contains_key(&dropped_key));
    }

    #[tokio::test]
    async fn disconnect_peer_reports_stale_generation_as_inactive() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) });

        let stale_generation = accepted_generation(mgr.activate_connection(HostName::new("peer"), sender, ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));

        let _current_generation = accepted_generation(mgr.activate_connection(
            HostName::new("peer"),
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }),
            ConnectionMeta {
                direction: ConnectionDirection::Outbound,
                config_label: None,
                expected_peer: Some(HostName::new("peer")),
                config_backed: true,
            },
        ));

        let plan = mgr.disconnect_peer(&HostName::new("peer"), stale_generation);

        assert!(!plan.was_active);
        assert!(mgr.current_generation(&HostName::new("peer")).is_some());
    }

    #[tokio::test]
    async fn failover_resync_for_relayed_origin_accepts_same_clock_snapshot() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let relay_a_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) });
        let relay_b_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) });
        let relay_a_generation = accepted_generation(mgr.activate_connection(HostName::new("relay-a"), relay_a_sender, ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));
        let relay_b_generation = accepted_generation(mgr.activate_connection(HostName::new("relay-b"), relay_b_sender, ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));

        let baseline = snapshot_msg("target", 1);
        let _ = mgr
            .handle_inbound(InboundPeerEnvelope {
                msg: PeerWireMessage::Data(baseline.clone()),
                connection_generation: relay_a_generation,
                connection_peer: HostName::new("relay-a"),
            })
            .await;
        mgr.routes.get_mut(&HostName::new("target")).expect("route exists").fallbacks.push(RouteHop {
            next_hop: HostName::new("relay-b"),
            next_hop_generation: relay_b_generation,
            learned_epoch: 10,
        });

        let plan = mgr.disconnect_peer(&HostName::new("relay-a"), relay_a_generation);
        let request_id = match &plan.resync_requests[0] {
            RoutedPeerMessage::RequestResync { request_id, .. } => *request_id,
            other => panic!("expected request_resync, got {:?}", other),
        };

        let result = mgr
            .handle_inbound(InboundPeerEnvelope {
                msg: PeerWireMessage::Routed(RoutedPeerMessage::ResyncSnapshot {
                    request_id,
                    requester_host: HostName::new("local"),
                    responder_host: HostName::new("target"),
                    remaining_hops: 4,
                    repo_identity: baseline.repo_identity.clone(),
                    repo_path: baseline.repo_path.clone(),
                    clock: baseline.clock.clone(),
                    seq: 1,
                    data: Box::new(ProviderData::default()),
                }),
                connection_generation: relay_b_generation,
                connection_peer: HostName::new("relay-b"),
            })
            .await;

        assert_eq!(result, HandleResult::Updated(test_repo()));
        let state = &mgr.get_peer_data()[&HostName::new("target")][&test_repo()];
        assert!(!state.stale);
        assert_eq!(state.via_peer, HostName::new("relay-b"));
    }

    #[tokio::test]
    async fn consecutive_failovers_reissue_resync_for_stale_snapshot() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let relay_a_generation = accepted_generation(mgr.activate_connection(
            HostName::new("relay-a"),
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }),
            ConnectionMeta { direction: ConnectionDirection::Inbound, config_label: None, expected_peer: None, config_backed: false },
        ));
        let relay_b_generation = accepted_generation(mgr.activate_connection(
            HostName::new("relay-b"),
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }),
            ConnectionMeta { direction: ConnectionDirection::Inbound, config_label: None, expected_peer: None, config_backed: false },
        ));
        let relay_c_generation = accepted_generation(mgr.activate_connection(
            HostName::new("relay-c"),
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }),
            ConnectionMeta { direction: ConnectionDirection::Inbound, config_label: None, expected_peer: None, config_backed: false },
        ));

        let baseline = snapshot_msg("target", 1);
        let _ = mgr
            .handle_inbound(InboundPeerEnvelope {
                msg: PeerWireMessage::Data(baseline.clone()),
                connection_generation: relay_a_generation,
                connection_peer: HostName::new("relay-a"),
            })
            .await;

        mgr.routes.get_mut(&HostName::new("target")).expect("route exists").fallbacks =
            vec![RouteHop { next_hop: HostName::new("relay-b"), next_hop_generation: relay_b_generation, learned_epoch: 10 }, RouteHop {
                next_hop: HostName::new("relay-c"),
                next_hop_generation: relay_c_generation,
                learned_epoch: 20,
            }];

        let first_plan = mgr.disconnect_peer(&HostName::new("relay-a"), relay_a_generation);
        assert_eq!(first_plan.resync_requests.len(), 1);
        let state = &mgr.get_peer_data()[&HostName::new("target")][&test_repo()];
        assert!(state.stale);

        let second_plan = mgr.disconnect_peer(&HostName::new("relay-c"), relay_c_generation);

        assert_eq!(second_plan.resync_requests.len(), 1);
        match &second_plan.resync_requests[0] {
            RoutedPeerMessage::RequestResync { target_host, .. } => {
                assert_eq!(target_host, &HostName::new("target"));
            }
            other => panic!("expected request_resync, got {:?}", other),
        }
        assert_eq!(mgr.routes[&HostName::new("target")].primary.next_hop, HostName::new("relay-b"));
    }

    #[tokio::test]
    async fn failover_resync_clears_stale_and_rebinds_provenance() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let direct_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) });
        let relay_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) });
        let direct_generation = accepted_generation(mgr.activate_connection(HostName::new("target"), direct_sender, ConnectionMeta {
            direction: ConnectionDirection::Outbound,
            config_label: None,
            expected_peer: Some(HostName::new("target")),
            config_backed: true,
        }));
        let relay_generation = accepted_generation(mgr.activate_connection(HostName::new("relay"), relay_sender, ConnectionMeta {
            direction: ConnectionDirection::Outbound,
            config_label: None,
            expected_peer: Some(HostName::new("relay")),
            config_backed: true,
        }));
        let baseline = snapshot_msg("target", 1);
        let _ = mgr
            .handle_inbound(InboundPeerEnvelope {
                msg: PeerWireMessage::Data(baseline.clone()),
                connection_generation: direct_generation,
                connection_peer: HostName::new("target"),
            })
            .await;

        mgr.routes.get_mut(&HostName::new("target")).expect("route exists").fallbacks.push(RouteHop {
            next_hop: HostName::new("relay"),
            next_hop_generation: relay_generation,
            learned_epoch: 10,
        });

        let plan = mgr.disconnect_peer(&HostName::new("target"), direct_generation);
        let request = match &plan.resync_requests[0] {
            RoutedPeerMessage::RequestResync { request_id, .. } => *request_id,
            other => panic!("expected request_resync, got {:?}", other),
        };

        let result = mgr
            .handle_inbound(InboundPeerEnvelope {
                msg: PeerWireMessage::Routed(RoutedPeerMessage::ResyncSnapshot {
                    request_id: request,
                    requester_host: HostName::new("local"),
                    responder_host: HostName::new("target"),
                    remaining_hops: 4,
                    repo_identity: baseline.repo_identity.clone(),
                    repo_path: baseline.repo_path.clone(),
                    clock: baseline.clock.clone(),
                    seq: 1,
                    data: Box::new(ProviderData::default()),
                }),
                connection_generation: relay_generation,
                connection_peer: HostName::new("relay"),
            })
            .await;

        assert_eq!(result, HandleResult::Updated(test_repo()));
        let state = &mgr.get_peer_data()[&HostName::new("target")][&test_repo()];
        assert!(!state.stale, "failover resync should clear stale");
        assert_eq!(state.via_peer, HostName::new("relay"));
        assert_eq!(state.via_generation, relay_generation);
    }

    #[tokio::test]
    async fn expired_resync_request_removes_stale_snapshot() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let relay_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) });
        let relay_generation = accepted_generation(mgr.activate_connection(HostName::new("relay"), relay_sender, ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));

        let _ = mgr
            .handle_inbound(InboundPeerEnvelope {
                msg: PeerWireMessage::Data(snapshot_msg("target", 1)),
                connection_generation: relay_generation,
                connection_peer: HostName::new("relay"),
            })
            .await;

        let state = mgr.peer_data.get_mut(&HostName::new("target")).and_then(|repos| repos.get_mut(&test_repo())).expect("repo state");
        state.stale = true;

        mgr.pending_resync_requests.insert(
            ReversePathKey {
                request_id: 7,
                requester_host: HostName::new("local"),
                target_host: HostName::new("target"),
                repo_identity: test_repo(),
            },
            PendingResyncRequest { deadline_at: Instant::now() - Duration::from_secs(1) },
        );

        let affected = mgr.sweep_expired_resyncs(Instant::now());

        assert_eq!(affected, vec![test_repo()]);
        assert!(!mgr.pending_resync_requests.iter().any(|(key, _)| key.request_id == 7));
        assert!(!mgr.peer_data.get(&HostName::new("target")).is_some_and(|repos| repos.contains_key(&test_repo())));
    }

    #[tokio::test]
    async fn disconnect_peer_returns_overlay_updates_for_remaining_peers() {
        let mut mgr = PeerManager::new(HostName::new("local"));

        handle_test_peer_data(&mut mgr, snapshot_msg("desktop", 1), || {
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
        })
        .await;
        handle_test_peer_data(&mut mgr, snapshot_msg("laptop", 1), || {
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
        })
        .await;

        let desktop_generation = mgr.current_generation(&HostName::new("desktop")).expect("desktop connected");

        let plan = mgr.disconnect_peer(&HostName::new("desktop"), desktop_generation);

        assert!(plan.was_active);
        assert_eq!(plan.overlay_updates.len(), 1);
        match &plan.overlay_updates[0] {
            OverlayUpdate::SetProviders { identity, peers } => {
                assert_eq!(identity, &test_repo());
                assert_eq!(peers.len(), 1);
                assert_eq!(peers[0].0, HostName::new("laptop"));
            }
            other => panic!("expected SetProviders, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn disconnect_peer_returns_remove_repo_for_remote_only_with_no_remaining_peers() {
        let mut mgr = PeerManager::new(HostName::new("local"));

        handle_test_peer_data(&mut mgr, snapshot_msg("desktop", 1), || {
            Arc::new(MockPeerSender { sent: Arc::new(Mutex::new(Vec::new())) }) as Arc<dyn PeerSender>
        })
        .await;

        let desktop_generation = mgr.current_generation(&HostName::new("desktop")).expect("desktop connected");

        let synthetic_path = PathBuf::from("/virtual/github.com/owner/repo");
        mgr.register_remote_repo(test_repo(), synthetic_path.clone());

        let plan = mgr.disconnect_peer(&HostName::new("desktop"), desktop_generation);

        assert!(plan.was_active);
        assert_eq!(plan.overlay_updates.len(), 1);
        match &plan.overlay_updates[0] {
            OverlayUpdate::RemoveRepo { identity, path } => {
                assert_eq!(identity, &test_repo());
                assert_eq!(path, &synthetic_path);
            }
            other => panic!("expected RemoveRepo, got {:?}", other),
        }
        assert!(!mgr.is_remote_repo(&test_repo()));
    }

    #[tokio::test]
    async fn get_sender_if_current_returns_sender_for_matching_generation() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender { sent: Arc::clone(&sent) });
        let generation = accepted_generation(mgr.activate_connection(HostName::new("peer"), sender, ConnectionMeta {
            direction: ConnectionDirection::Inbound,
            config_label: None,
            expected_peer: None,
            config_backed: false,
        }));

        assert!(mgr.get_sender_if_current(&HostName::new("peer"), generation).is_some());
        assert!(mgr.get_sender_if_current(&HostName::new("peer"), generation + 1).is_none());
        assert!(mgr.get_sender_if_current(&HostName::new("unknown"), 1).is_none());
    }
}
