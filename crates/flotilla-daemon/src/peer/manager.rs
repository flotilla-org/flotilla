use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use flotilla_protocol::{
    ConfigLabel, HostName, PeerDataKind, PeerDataMessage, PeerWireMessage, ProviderData,
    RepoIdentity, RoutedPeerMessage, VectorClock,
};

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
    ResyncRequested {
        request_id: u64,
        from: HostName,
        repo: RepoIdentity,
        since_seq: u64,
    },
    /// A delta was received but cannot be applied (seq gap or not yet implemented).
    /// Caller should request a full resync from the origin.
    NeedsResync { from: HostName, repo: RepoIdentity },
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

#[derive(Debug, Clone)]
pub struct DisconnectPlan {
    pub affected_repos: Vec<RepoIdentity>,
    pub resync_requests: Vec<RoutedPeerMessage>,
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
    transport_peers: HashMap<ConfigLabel, HostName>,
    generations: HashMap<HostName, u64>,
    routes: HashMap<HostName, RouteState>,
    reverse_paths: HashMap<ReversePathKey, ReversePathHop>,
    pending_resync_requests: HashMap<ReversePathKey, PendingResyncRequest>,
    route_epoch: u64,
    request_id_counter: u64,
    peer_data: HashMap<HostName, HashMap<RepoIdentity, PerRepoPeerState>>,
    /// RepoIdentity values that exist only on remote peers — no local repo
    /// matches. Each maps to the synthetic path used for tab identity.
    known_remote_repos: HashMap<RepoIdentity, PathBuf>,
    /// Last-seen vector clock per (origin_host, repo_identity) — used to
    /// detect and drop duplicate / already-seen messages.
    last_seen_clocks: HashMap<(HostName, RepoIdentity), VectorClock>,
}

impl PeerManager {
    const RESYNC_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
    const DEFAULT_ROUTED_HOPS: u8 = 8;

    /// Create a new PeerManager with no peers.
    pub fn new(local_host: HostName) -> Self {
        Self {
            local_host,
            peers: HashMap::new(),
            senders: HashMap::new(),
            transport_peers: HashMap::new(),
            generations: HashMap::new(),
            routes: HashMap::new(),
            reverse_paths: HashMap::new(),
            pending_resync_requests: HashMap::new(),
            route_epoch: 0,
            request_id_counter: 0,
            peer_data: HashMap::new(),
            known_remote_repos: HashMap::new(),
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

    pub fn note_pending_resync_request(
        &mut self,
        target_host: HostName,
        repo_identity: RepoIdentity,
    ) -> u64 {
        let request_id = self.next_request_id();
        self.pending_resync_requests.insert(
            ReversePathKey {
                request_id,
                requester_host: self.local_host.clone(),
                target_host,
                repo_identity,
            },
            PendingResyncRequest {
                deadline_at: Instant::now() + Self::RESYNC_REQUEST_TIMEOUT,
            },
        );
        request_id
    }

    pub fn current_generation(&self, name: &HostName) -> Option<u64> {
        self.generations.get(name).copied()
    }

    fn generation_is_current(&self, name: &HostName, generation: u64) -> bool {
        generation != 0 && self.generations.get(name).copied() == Some(generation)
    }

    fn install_direct_route(&mut self, host: &HostName, generation: u64) {
        let learned_epoch = self.next_route_epoch();
        self.routes.insert(
            host.clone(),
            RouteState {
                primary: RouteHop {
                    next_hop: host.clone(),
                    next_hop_generation: generation,
                    learned_epoch,
                },
                fallbacks: Vec::new(),
                candidates: Vec::new(),
            },
        );
    }

    fn route_hop_is_live(&self, hop: &RouteHop) -> bool {
        self.generation_is_current(&hop.next_hop, hop.next_hop_generation)
            && self.senders.contains_key(&hop.next_hop)
    }

    fn promote_route_after_disconnect(&mut self, origin: &HostName) -> Option<RouteHop> {
        let mut route = self.routes.remove(origin)?;

        route
            .fallbacks
            .retain(|hop| self.route_hop_is_live(hop) && hop.next_hop != *origin);
        route
            .candidates
            .retain(|hop| self.route_hop_is_live(hop) && hop.next_hop != *origin);

        if self.route_hop_is_live(&route.primary) && route.primary.next_hop != *origin {
            let primary = route.primary.clone();
            self.routes.insert(origin.clone(), route);
            return Some(primary);
        }

        if let Some((idx, _)) = route
            .fallbacks
            .iter()
            .enumerate()
            .max_by_key(|(_, hop)| hop.learned_epoch)
        {
            let next = route.fallbacks.remove(idx);
            route.primary = next.clone();
            self.routes.insert(origin.clone(), route);
            return Some(next);
        }

        self.routes.remove(origin);
        None
    }

    pub fn activate_connection(
        &mut self,
        host: HostName,
        sender: Arc<dyn PeerSender>,
        meta: ConnectionMeta,
    ) -> u64 {
        let generation = self
            .generations
            .get(&host)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        self.generations.insert(host.clone(), generation);
        self.senders.insert(host.clone(), sender);
        self.install_direct_route(&host, generation);

        if let Some(label) = meta.config_label {
            self.transport_peers.insert(label, host);
        }

        generation
    }

    fn store_snapshot_from(
        &mut self,
        via_peer: &HostName,
        via_generation: u64,
        msg: PeerDataMessage,
    ) -> HandleResult {
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

        self.last_seen_clocks
            .entry(dedup_key)
            .or_default()
            .merge(&msg.clock);

        match msg.kind {
            PeerDataKind::Snapshot { data, seq } => {
                let repo_states = self.peer_data.entry(origin.clone()).or_default();
                repo_states.insert(
                    repo.clone(),
                    PerRepoPeerState {
                        provider_data: *data,
                        repo_path,
                        seq,
                        via_peer: via_peer.clone(),
                        via_generation,
                        stale: false,
                    },
                );

                if !self.routes.contains_key(&origin) {
                    let learned_epoch = self.next_route_epoch();
                    self.routes.insert(
                        origin.clone(),
                        RouteState {
                            primary: RouteHop {
                                next_hop: via_peer.clone(),
                                next_hop_generation: via_generation,
                                learned_epoch,
                            },
                            fallbacks: Vec::new(),
                            candidates: Vec::new(),
                        },
                    );
                }

                HandleResult::Updated(repo)
            }
            PeerDataKind::Delta {
                seq,
                prev_seq,
                changes: _,
            } => {
                debug!(
                    origin = %origin,
                    repo = %repo,
                    %seq,
                    %prev_seq,
                    "received peer delta, requesting resync (delta application not yet implemented)"
                );

                HandleResult::NeedsResync { from: origin, repo }
            }
            PeerDataKind::RequestResync { since_seq } => HandleResult::ResyncRequested {
                request_id: 0,
                from: origin,
                repo,
                since_seq,
            },
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
            PeerWireMessage::Routed(msg) => {
                self.handle_routed(env.connection_peer, env.connection_generation, msg)
                    .await
            }
        }
    }

    async fn handle_routed(
        &mut self,
        connection_peer: HostName,
        connection_generation: u64,
        msg: RoutedPeerMessage,
    ) -> HandleResult {
        match msg {
            RoutedPeerMessage::RequestResync {
                request_id,
                requester_host,
                target_host,
                remaining_hops,
                repo_identity,
                since_seq,
            } => {
                if remaining_hops == 0 {
                    return HandleResult::Ignored;
                }
                if target_host == self.local_host {
                    return HandleResult::ResyncRequested {
                        request_id,
                        from: requester_host,
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
                self.reverse_paths.insert(
                    key,
                    ReversePathHop {
                        next_hop: connection_peer,
                        next_hop_generation: connection_generation,
                        learned_at,
                    },
                );

                let forwarded = RoutedPeerMessage::RequestResync {
                    request_id,
                    requester_host,
                    target_host: target_host.clone(),
                    remaining_hops: remaining_hops.saturating_sub(1),
                    repo_identity,
                    since_seq,
                };
                let _ = self
                    .send_to(&target_host, PeerWireMessage::Routed(forwarded))
                    .await;
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
                    return self.store_snapshot_from(
                        &connection_peer,
                        connection_generation,
                        PeerDataMessage {
                            origin_host: responder_host,
                            repo_identity,
                            repo_path,
                            clock,
                            kind: PeerDataKind::Snapshot { data, seq },
                        },
                    );
                }

                if remaining_hops == 0 {
                    return HandleResult::Ignored;
                }

                let Some(reverse_hop) = self.reverse_paths.get(&key).cloned() else {
                    return HandleResult::Ignored;
                };
                if !self
                    .generation_is_current(&reverse_hop.next_hop, reverse_hop.next_hop_generation)
                {
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
        }
    }

    /// Process an inbound PeerDataMessage.
    ///
    /// - Snapshot: stores provider_data and seq, returns Updated.
    /// - Delta: for Phase 1 we don't apply deltas, so we return NeedsResync.
    /// - RequestResync: returns ResyncRequested so the caller can send a snapshot.
    pub fn handle_peer_data(&mut self, msg: PeerDataMessage) -> HandleResult {
        let origin = msg.origin_host.clone();
        if origin == self.local_host {
            debug!(host = %origin, "ignoring peer data from self");
            return HandleResult::Ignored;
        }
        let generation = self.generations.get(&origin).copied().unwrap_or(1);
        self.store_snapshot_from(&origin, generation, msg)
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

            match sender
                .send(PeerWireMessage::Data(relayed_msg.clone()))
                .await
            {
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
                        Ok((sender, subscribe_result))
                    }
                    Err(e) => Err(e),
                }
            } else {
                continue;
            };

            match connect_result {
                Ok((sender, subscribe_result)) => {
                    info!(peer = %name, "peer transport connected");
                    let mut generation = 0;
                    if let Some(sender) = sender {
                        generation = self.activate_connection(
                            name.clone(),
                            sender,
                            ConnectionMeta {
                                direction: ConnectionDirection::Outbound,
                                config_label: None,
                                expected_peer: Some(name.clone()),
                            },
                        );
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

    /// Return the currently addressable peers that have active senders.
    pub fn active_peers(&self) -> Vec<HostName> {
        self.senders.keys().cloned().collect()
    }

    /// Remove all stored data for a peer (e.g. on disconnect).
    ///
    /// Returns the list of RepoIdentity values that were affected, so the
    /// caller can rebuild the daemon's peer overlay for those repos.
    pub fn remove_peer_data(&mut self, name: &HostName) -> Vec<RepoIdentity> {
        let affected: Vec<RepoIdentity> = self
            .peer_data
            .get(name)
            .map(|repos| repos.keys().cloned().collect())
            .unwrap_or_default();
        self.peer_data.remove(name);
        self.last_seen_clocks.retain(|(host, _), _| host != name);
        info!(peer = %name, repos = affected.len(), "cleared peer data");
        affected
    }

    /// Check whether a remote-only repo still has any peer data backing it.
    ///
    /// Returns `true` if at least one remaining peer holds data for this identity.
    pub fn has_peer_data_for(&self, identity: &RepoIdentity) -> bool {
        self.peer_data
            .values()
            .any(|repos| repos.contains_key(identity))
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
        if let Some(sender) = self.senders.get(name) {
            return sender.send(msg).await;
        }

        let route = self
            .routes
            .get(name)
            .ok_or_else(|| format!("unknown peer: {name}"))?;
        let sender = self
            .senders
            .get(&route.primary.next_hop)
            .ok_or_else(|| format!("missing next hop sender: {}", route.primary.next_hop))?;
        sender.send(msg).await
    }

    /// Reconnect a specific peer: disconnect, then connect + subscribe.
    ///
    /// Returns the new inbound receiver on success.
    pub async fn reconnect_peer(
        &mut self,
        name: &HostName,
    ) -> Result<(u64, mpsc::Receiver<PeerWireMessage>), String> {
        let (sender, rx) = {
            let transport = self
                .peers
                .get_mut(name)
                .ok_or_else(|| format!("unknown peer: {name}"))?;

            // Best-effort disconnect before reconnecting
            let _ = transport.disconnect().await;

            transport.connect().await?;
            let sender = transport.sender();
            let rx = transport.subscribe().await?;
            (sender, rx)
        };

        let mut generation = 0;
        if let Some(sender) = sender {
            generation = self.activate_connection(
                name.clone(),
                sender,
                ConnectionMeta {
                    direction: ConnectionDirection::Outbound,
                    config_label: None,
                    expected_peer: Some(name.clone()),
                },
            );
        }

        Ok((generation, rx))
    }

    pub fn disconnect_peer(&mut self, name: &HostName, generation: u64) -> DisconnectPlan {
        if !self.generation_is_current(name, generation) {
            return DisconnectPlan {
                affected_repos: Vec::new(),
                resync_requests: Vec::new(),
            };
        }

        self.senders.remove(name);
        self.generations.remove(name);
        self.reverse_paths.retain(|_, hop| hop.next_hop != *name);
        self.pending_resync_requests
            .retain(|key, _| key.target_host != *name);

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
                        .filter(|(_, state)| {
                            state.via_peer == *name && state.via_generation == generation
                        })
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
                    self.pending_resync_requests.insert(
                        key,
                        PendingResyncRequest {
                            deadline_at: Instant::now() + Self::RESYNC_REQUEST_TIMEOUT,
                        },
                    );
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

        DisconnectPlan {
            affected_repos,
            resync_requests,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tokio::sync::mpsc;

    use super::super::transport::PeerConnectionStatus;

    struct MockPeerSender {
        sent: Arc<Mutex<Vec<PeerWireMessage>>>,
    }

    #[async_trait]
    impl PeerSender for MockPeerSender {
        async fn send(&self, msg: PeerWireMessage) -> Result<(), String> {
            self.sent.lock().expect("lock poisoned").push(msg);
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
            Self {
                status: PeerConnectionStatus::Connected,
                sender: None,
            }
        }

        fn with_sender() -> (Self, Arc<Mutex<Vec<PeerWireMessage>>>) {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender {
                sent: Arc::clone(&sent),
            });
            let transport = Self {
                status: PeerConnectionStatus::Connected,
                sender: Some(sender),
            };
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
        RepoIdentity {
            authority: "github.com".into(),
            path: "owner/repo".into(),
        }
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
            kind: PeerDataKind::Snapshot {
                data: Box::new(ProviderData::default()),
                seq,
            },
        }
    }

    #[test]
    fn handle_snapshot_stores_data() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let msg = snapshot_msg("remote", 1);

        let result = mgr.handle_peer_data(msg);
        assert_eq!(result, HandleResult::Updated(test_repo()));

        let peer_data = mgr.get_peer_data();
        let remote_host = HostName::new("remote");
        assert!(peer_data.contains_key(&remote_host));
        let repo_state = &peer_data[&remote_host][&test_repo()];
        assert_eq!(repo_state.seq, 1);
        assert_eq!(repo_state.repo_path, PathBuf::from("/home/dev/repo"));
    }

    #[test]
    fn handle_snapshot_updates_existing_data() {
        let mut mgr = PeerManager::new(HostName::new("local"));

        // First snapshot
        let msg1 = snapshot_msg("remote", 1);
        mgr.handle_peer_data(msg1);

        // Second snapshot with higher seq
        let msg2 = snapshot_msg("remote", 5);
        let result = mgr.handle_peer_data(msg2);
        assert_eq!(result, HandleResult::Updated(test_repo()));

        let peer_data = mgr.get_peer_data();
        let repo_state = &peer_data[&HostName::new("remote")][&test_repo()];
        assert_eq!(repo_state.seq, 5);
    }

    #[test]
    fn handle_request_resync_returns_resync_requested() {
        let mut mgr = PeerManager::new(HostName::new("local"));

        let msg = PeerDataMessage {
            origin_host: HostName::new("remote"),
            repo_identity: test_repo(),
            repo_path: PathBuf::from("/home/dev/repo"),
            clock: VectorClock::default(),
            kind: PeerDataKind::RequestResync { since_seq: 3 },
        };

        let result = mgr.handle_peer_data(msg);
        assert_eq!(
            result,
            HandleResult::ResyncRequested {
                request_id: 0,
                from: HostName::new("remote"),
                repo: test_repo(),
                since_seq: 3,
            }
        );
    }

    #[test]
    fn handle_delta_returns_needs_resync() {
        use flotilla_protocol::delta::{Branch, BranchStatus, EntryOp};
        use flotilla_protocol::Change;

        let mut mgr = PeerManager::new(HostName::new("local"));

        let msg = PeerDataMessage {
            origin_host: HostName::new("remote"),
            repo_identity: test_repo(),
            repo_path: PathBuf::from("/home/dev/repo"),
            clock: VectorClock::default(),
            kind: PeerDataKind::Delta {
                changes: vec![Change::Branch {
                    key: "feat-x".into(),
                    op: EntryOp::Added(Branch {
                        status: BranchStatus::Remote,
                    }),
                }],
                seq: 2,
                prev_seq: 1,
            },
        };

        let result = mgr.handle_peer_data(msg);
        assert_eq!(
            result,
            HandleResult::NeedsResync {
                from: HostName::new("remote"),
                repo: test_repo(),
            }
        );
    }

    #[test]
    fn handle_ignores_messages_from_self() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let msg = snapshot_msg("local", 1);

        let result = mgr.handle_peer_data(msg);
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
            kind: PeerDataKind::Snapshot {
                data: Box::new(ProviderData::default()),
                seq: 1,
            },
        };

        mgr.relay(&HostName::new("F1"), &msg).await;

        // Leader is already in the clock, so relay should skip it
        assert!(
            sent_leader.lock().expect("lock").is_empty(),
            "should not relay back to a peer already in the clock"
        );
    }

    #[test]
    fn get_peer_data_returns_stored_data() {
        let mut mgr = PeerManager::new(HostName::new("local"));

        // Initially empty
        assert!(mgr.get_peer_data().is_empty());

        // After storing data from two hosts
        mgr.handle_peer_data(snapshot_msg("desktop", 1));
        mgr.handle_peer_data(snapshot_msg("server", 2));

        let data = mgr.get_peer_data();
        assert_eq!(data.len(), 2);
        assert!(data.contains_key(&HostName::new("desktop")));
        assert!(data.contains_key(&HostName::new("server")));
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
        let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender {
            sent: Arc::clone(&sent),
        });
        mgr.register_sender(HostName::new("peer"), sender);

        mgr.send_to(
            &HostName::new("peer"),
            PeerWireMessage::Data(snapshot_msg("local", 1)),
        )
        .await
        .expect("send succeeds");

        assert_eq!(sent.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn activate_connection_supersedes_older_sender() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let first_sent = Arc::new(Mutex::new(Vec::new()));
        let second_sent = Arc::new(Mutex::new(Vec::new()));
        let first_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender {
            sent: Arc::clone(&first_sent),
        });
        let second_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender {
            sent: Arc::clone(&second_sent),
        });

        let gen1 = mgr.activate_connection(
            HostName::new("peer"),
            first_sender,
            ConnectionMeta {
                direction: ConnectionDirection::Inbound,
                config_label: None,
                expected_peer: None,
            },
        );
        let gen2 = mgr.activate_connection(
            HostName::new("peer"),
            second_sender,
            ConnectionMeta {
                direction: ConnectionDirection::Inbound,
                config_label: None,
                expected_peer: None,
            },
        );

        assert_eq!(gen1, 1);
        assert_eq!(gen2, 2);
        mgr.send_to(
            &HostName::new("peer"),
            PeerWireMessage::Data(snapshot_msg("local", 1)),
        )
        .await
        .expect("send succeeds");

        assert!(first_sent.lock().expect("lock").is_empty());
        assert_eq!(second_sent.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn stale_generation_inbound_message_is_dropped() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender {
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        let generation = mgr.activate_connection(
            HostName::new("peer"),
            sender,
            ConnectionMeta {
                direction: ConnectionDirection::Inbound,
                config_label: None,
                expected_peer: None,
            },
        );
        assert_eq!(generation, 1);
        let _ = mgr.activate_connection(
            HostName::new("peer"),
            Arc::new(MockPeerSender {
                sent: Arc::new(Mutex::new(Vec::new())),
            }),
            ConnectionMeta {
                direction: ConnectionDirection::Inbound,
                config_label: None,
                expected_peer: None,
            },
        );

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
        let via_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender {
            sent: Arc::clone(&sent),
        });
        mgr.register_sender(HostName::new("relay"), via_sender);
        mgr.generations.insert(HostName::new("relay"), 1);
        mgr.routes.insert(
            HostName::new("target"),
            RouteState {
                primary: RouteHop {
                    next_hop: HostName::new("relay"),
                    next_hop_generation: 1,
                    learned_epoch: 1,
                },
                fallbacks: Vec::new(),
                candidates: Vec::new(),
            },
        );

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
            .send_to(
                &HostName::new("missing"),
                PeerWireMessage::Data(snapshot_msg("local", 1)),
            )
            .await
            .expect_err("missing route should error");
        assert!(err.contains("unknown peer"));
    }

    #[tokio::test]
    async fn late_resync_snapshot_is_dropped_without_pending_request() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender {
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        let generation = mgr.activate_connection(
            HostName::new("relay"),
            sender,
            ConnectionMeta {
                direction: ConnectionDirection::Inbound,
                config_label: None,
                expected_peer: None,
            },
        );

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
    async fn routed_request_resync_is_dropped_when_hop_budget_exhausted() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender {
            sent: Arc::clone(&sent),
        });
        let generation = mgr.activate_connection(
            HostName::new("relay"),
            sender,
            ConnectionMeta {
                direction: ConnectionDirection::Inbound,
                config_label: None,
                expected_peer: None,
            },
        );

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
        let sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender {
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        let generation = mgr.activate_connection(
            HostName::new("relay"),
            sender,
            ConnectionMeta {
                direction: ConnectionDirection::Inbound,
                config_label: None,
                expected_peer: None,
            },
        );

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

        assert_eq!(
            result,
            HandleResult::ResyncRequested {
                request_id: 41,
                from: HostName::new("requester"),
                repo: test_repo(),
                since_seq: 7,
            }
        );
    }

    #[tokio::test]
    async fn disconnect_peer_keeps_snapshot_stale_when_fallback_exists() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let direct_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender {
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        let relay_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender {
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        let direct_generation = mgr.activate_connection(
            HostName::new("target"),
            direct_sender,
            ConnectionMeta {
                direction: ConnectionDirection::Outbound,
                config_label: None,
                expected_peer: Some(HostName::new("target")),
            },
        );
        let relay_generation = mgr.activate_connection(
            HostName::new("relay"),
            relay_sender,
            ConnectionMeta {
                direction: ConnectionDirection::Outbound,
                config_label: None,
                expected_peer: Some(HostName::new("relay")),
            },
        );
        let _ = mgr
            .handle_inbound(InboundPeerEnvelope {
                msg: PeerWireMessage::Data(snapshot_msg("target", 1)),
                connection_generation: direct_generation,
                connection_peer: HostName::new("target"),
            })
            .await;

        mgr.routes
            .get_mut(&HostName::new("target"))
            .expect("route exists")
            .fallbacks
            .push(RouteHop {
                next_hop: HostName::new("relay"),
                next_hop_generation: relay_generation,
                learned_epoch: 10,
            });

        let plan = mgr.disconnect_peer(&HostName::new("target"), direct_generation);

        assert_eq!(plan.affected_repos, vec![test_repo()]);
        assert_eq!(plan.resync_requests.len(), 1);
        let state = &mgr.get_peer_data()[&HostName::new("target")][&test_repo()];
        assert!(state.stale, "snapshot should be retained as stale");
        assert_eq!(
            mgr.routes[&HostName::new("target")].primary.next_hop,
            HostName::new("relay")
        );
    }

    #[tokio::test]
    async fn disconnect_peer_keeps_unrelated_pending_resync_requests() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let target_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender {
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        let other_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender {
            sent: Arc::new(Mutex::new(Vec::new())),
        });

        mgr.activate_connection(
            HostName::new("target"),
            target_sender,
            ConnectionMeta {
                direction: ConnectionDirection::Inbound,
                config_label: None,
                expected_peer: None,
            },
        );
        let other_generation = mgr.activate_connection(
            HostName::new("other"),
            other_sender,
            ConnectionMeta {
                direction: ConnectionDirection::Inbound,
                config_label: None,
                expected_peer: None,
            },
        );

        let kept_request_id = mgr.note_pending_resync_request(HostName::new("target"), test_repo());
        let dropped_request_id =
            mgr.note_pending_resync_request(HostName::new("other"), test_repo());

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
    async fn failover_resync_clears_stale_and_rebinds_provenance() {
        let mut mgr = PeerManager::new(HostName::new("local"));
        let direct_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender {
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        let relay_sender: Arc<dyn PeerSender> = Arc::new(MockPeerSender {
            sent: Arc::new(Mutex::new(Vec::new())),
        });
        let direct_generation = mgr.activate_connection(
            HostName::new("target"),
            direct_sender,
            ConnectionMeta {
                direction: ConnectionDirection::Outbound,
                config_label: None,
                expected_peer: Some(HostName::new("target")),
            },
        );
        let relay_generation = mgr.activate_connection(
            HostName::new("relay"),
            relay_sender,
            ConnectionMeta {
                direction: ConnectionDirection::Outbound,
                config_label: None,
                expected_peer: Some(HostName::new("relay")),
            },
        );
        let baseline = snapshot_msg("target", 1);
        let _ = mgr
            .handle_inbound(InboundPeerEnvelope {
                msg: PeerWireMessage::Data(baseline.clone()),
                connection_generation: direct_generation,
                connection_peer: HostName::new("target"),
            })
            .await;

        mgr.routes
            .get_mut(&HostName::new("target"))
            .expect("route exists")
            .fallbacks
            .push(RouteHop {
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
}
