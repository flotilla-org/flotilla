use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use flotilla_protocol::{
    HostName, PeerDataKind, PeerDataMessage, ProviderData, RepoIdentity, VectorClock,
};

use super::transport::PeerTransport;

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

/// Per-repo state received from a single peer host.
pub struct PerRepoPeerState {
    pub provider_data: ProviderData,
    pub repo_path: PathBuf,
    pub seq: u64,
}

/// Manages connections to remote peer hosts and stores their provider data.
///
/// The PeerManager does NOT own the InProcessDaemon. It returns information
/// about what changed so the caller (DaemonServer / wiring code) can trigger
/// re-merge on the daemon.
pub struct PeerManager {
    local_host: HostName,
    peers: HashMap<HostName, Box<dyn PeerTransport>>,
    peer_data: HashMap<HostName, HashMap<RepoIdentity, PerRepoPeerState>>,
    /// RepoIdentity values that exist only on remote peers — no local repo
    /// matches. Each maps to the synthetic path used for tab identity.
    known_remote_repos: HashMap<RepoIdentity, PathBuf>,
    /// Last-seen vector clock per (origin_host, repo_identity) — used to
    /// detect and drop duplicate / already-seen messages.
    last_seen_clocks: HashMap<(HostName, RepoIdentity), VectorClock>,
}

impl PeerManager {
    /// Create a new PeerManager with no peers.
    pub fn new(local_host: HostName) -> Self {
        Self {
            local_host,
            peers: HashMap::new(),
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

    /// Process an inbound PeerDataMessage.
    ///
    /// - Snapshot: stores provider_data and seq, returns Updated.
    /// - Delta: for Phase 1 we don't apply deltas, so we return NeedsResync.
    /// - RequestResync: returns ResyncRequested so the caller can send a snapshot.
    pub fn handle_peer_data(&mut self, msg: PeerDataMessage) -> HandleResult {
        let origin = msg.origin_host.clone();
        let repo = msg.repo_identity.clone();
        let repo_path = msg.repo_path.clone();

        // Ignore messages from ourselves
        if origin == self.local_host {
            debug!(host = %origin, "ignoring peer data from self");
            return HandleResult::Ignored;
        }

        // Vector clock dedup: drop if the incoming clock is dominated by
        // (i.e. ≤ in every entry) our last-seen clock for this origin+repo.
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

        // Update last-seen clock (merge to handle concurrent paths)
        self.last_seen_clocks
            .entry(dedup_key)
            .or_default()
            .merge(&msg.clock);

        match msg.kind {
            PeerDataKind::Snapshot { data, seq } => {
                debug!(
                    origin = %origin,
                    repo = %repo,
                    %seq,
                    "received peer snapshot"
                );

                let repo_states = self.peer_data.entry(origin).or_default();
                repo_states.insert(
                    repo.clone(),
                    PerRepoPeerState {
                        provider_data: *data,
                        repo_path,
                        seq,
                    },
                );

                HandleResult::Updated(repo)
            }
            PeerDataKind::Delta {
                seq,
                prev_seq,
                changes: _,
            } => {
                // Phase 1: we don't apply deltas. Check if we have state and
                // whether the seq is contiguous. Either way, request resync.
                debug!(
                    origin = %origin,
                    repo = %repo,
                    %seq,
                    %prev_seq,
                    "received peer delta, requesting resync (delta application not yet implemented)"
                );

                HandleResult::NeedsResync { from: origin, repo }
            }
            PeerDataKind::RequestResync { since_seq } => {
                debug!(
                    from = %origin,
                    repo = %repo,
                    %since_seq,
                    "peer requested resync"
                );

                HandleResult::ResyncRequested {
                    from: origin,
                    repo,
                    since_seq,
                }
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

        for (name, transport) in &self.peers {
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

            match transport.send(relayed_msg.clone()).await {
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
    pub async fn connect_all(&mut self) -> Vec<(HostName, mpsc::Receiver<PeerDataMessage>)> {
        let names: Vec<HostName> = self.peers.keys().cloned().collect();
        let mut receivers = Vec::new();
        for name in names {
            if let Some(transport) = self.peers.get_mut(&name) {
                match transport.connect().await {
                    Ok(()) => {
                        info!(peer = %name, "peer transport connected");
                        match transport.subscribe().await {
                            Ok(rx) => {
                                receivers.push((name.clone(), rx));
                            }
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
    pub async fn send_to(&self, name: &HostName, msg: PeerDataMessage) -> Result<(), String> {
        let transport = self
            .peers
            .get(name)
            .ok_or_else(|| format!("unknown peer: {name}"))?;
        transport.send(msg).await
    }

    /// Reconnect a specific peer: disconnect, then connect + subscribe.
    ///
    /// Returns the new inbound receiver on success.
    pub async fn reconnect_peer(
        &mut self,
        name: &HostName,
    ) -> Result<mpsc::Receiver<PeerDataMessage>, String> {
        let transport = self
            .peers
            .get_mut(name)
            .ok_or_else(|| format!("unknown peer: {name}"))?;

        // Best-effort disconnect before reconnecting
        let _ = transport.disconnect().await;

        transport.connect().await?;
        transport.subscribe().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tokio::sync::mpsc;

    use super::super::transport::PeerConnectionStatus;

    /// Mock transport that records sent messages and tracks connection status.
    struct MockTransport {
        sent: Arc<Mutex<Vec<PeerDataMessage>>>,
        status: PeerConnectionStatus,
    }

    impl MockTransport {
        fn new() -> (Self, Arc<Mutex<Vec<PeerDataMessage>>>) {
            let sent = Arc::new(Mutex::new(Vec::new()));
            let transport = Self {
                sent: Arc::clone(&sent),
                status: PeerConnectionStatus::Connected,
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

        async fn subscribe(&mut self) -> Result<mpsc::Receiver<PeerDataMessage>, String> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        }

        async fn send(&self, msg: PeerDataMessage) -> Result<(), String> {
            self.sent.lock().expect("lock poisoned").push(msg);
            Ok(())
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

        let (transport_a, sent_a) = MockTransport::new();
        let (transport_b, sent_b) = MockTransport::new();
        let (transport_c, sent_c) = MockTransport::new();

        mgr.add_peer(HostName::new("peer-a"), Box::new(transport_a));
        mgr.add_peer(HostName::new("peer-b"), Box::new(transport_b));
        mgr.add_peer(HostName::new("peer-c"), Box::new(transport_c));

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

        let (transport, sent) = MockTransport::new();
        mgr.add_peer(HostName::new("local"), Box::new(transport));

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

        let (transport_leader, sent_leader) = MockTransport::new();
        mgr.add_peer(HostName::new("leader"), Box::new(transport_leader));

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

        let (transport, _sent) = MockTransport::new();
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

        let (transport, _sent) = MockTransport::new();
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
}
