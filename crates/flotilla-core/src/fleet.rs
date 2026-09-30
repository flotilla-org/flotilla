//! Fleet replica federation and its SSH snapshot transport.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_protocol::{
    arg::{flatten, Arg},
    result_set::{ConvoyRow, ResultSet, Rows},
    CanonicalHostId, CredentialAttention, CredentialAttentionSeverity, CrewAttention, FleetListRow, FleetObservationAgreement,
    FleetReplicaSnapshot, FleetStaleness, HostName, NodeId, PeerConnectionState,
};
use flotilla_resources::{
    Checkout as ResourceCheckout, Convoy as ResourceConvoy, Environment as ResourceEnvironment, Host as ResourceHost,
    HostStatus as ResourceHostStatus, ReadResourceObject, ResourceBackend, ResourceProvenance, TerminalAttentionState,
    TerminalSession as ResourceTerminalSession, TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionStatus, CONVOY_LABEL,
    ROLE_LABEL, VESSEL_LABEL,
};
use futures::future::join_all;
use tokio::sync::{broadcast, RwLock};

use crate::{
    aggregator_projection::AggregatorProjectionState,
    config::{ConfigStore, RemoteHostConfig},
    event_sink::EventSink,
    host_registry::HostRegistry,
    host_resolution::canonical_placement_host_ref_from_sources,
    providers::{ChannelLabel, CommandRunner},
};

pub(crate) const FLEET_REPLICA_FRESH_SECS: i64 = 90;

pub(crate) fn replica_sync_is_fresh(last_sync: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now.signed_duration_since(last_sync).num_seconds() <= FLEET_REPLICA_FRESH_SECS
}

pub(crate) async fn is_host_self_report(source: &ReadResourceObject<ResourceHost>, host_registry: &HostRegistry) -> bool {
    let ResourceProvenance::Replica { origin_root, .. } = &source.provenance else { return false };
    host_registry
        .environment_id_for_node(origin_root)
        .await
        .and_then(|environment_id| environment_id.host_id().map(ToString::to_string))
        .is_some_and(|host_id| host_id == source.object.metadata.name)
}
const FLEET_REPLICA_REFRESH_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) enum FleetRowSource {
    Local,
    IncludingReplicas,
}

#[async_trait]
pub(crate) trait FleetReplicaTransport: Send + Sync {
    async fn fetch(
        &self,
        remote: &RemoteHostConfig,
        multiplex: bool,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<ParsedFleetReplicaSnapshot, String>;
}

pub(crate) struct SshFleetReplicaTransport;

#[async_trait]
impl FleetReplicaTransport for SshFleetReplicaTransport {
    async fn fetch(
        &self,
        remote: &RemoteHostConfig,
        multiplex: bool,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<ParsedFleetReplicaSnapshot, String> {
        let args = fleet_replica_ssh_args(remote, multiplex);
        let arg_refs: Vec<_> = args.iter().map(String::as_str).collect();
        let output = tokio::time::timeout(
            FLEET_REPLICA_REFRESH_TIMEOUT,
            runner.run_output("ssh", &arg_refs, Path::new("/"), &ChannelLabel::Default),
        )
        .await
        .map_err(|_| format!("replica snapshot timed out after {}s", FLEET_REPLICA_REFRESH_TIMEOUT.as_secs()))?
        .map_err(|err| format!("replica snapshot ssh failed: {err}"))?;
        if !output.success {
            let message = if output.stderr.trim().is_empty() { output.stdout.trim() } else { output.stderr.trim() };
            return Err(format!("replica snapshot command failed: {message}"));
        }
        parse_fleet_replica_snapshot(output.stdout.trim())
    }
}

pub(crate) struct FleetService {
    // Retained for federation events introduced by later slices.
    _event_sink: Arc<dyn EventSink>,
    config: Arc<ConfigStore>,
    resource_backend: ResourceBackend,
    observed_resource_backend: ResourceBackend,
    aggregator_projection_state: AggregatorProjectionState,
    host_name: HostName,
    canonical_local_host_id: Option<CanonicalHostId>,
    transport: Arc<dyn FleetReplicaTransport>,
    fleet_replica_cache: RwLock<HashMap<HostName, FleetReplicaCacheEntry>>,
    fleet_replica_tx: broadcast::Sender<Vec<FleetReplicaSnapshot>>,
    resource_replication_failures: RwLock<HashMap<NodeId, BTreeMap<String, String>>>,
}

impl FleetService {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        event_sink: Arc<dyn EventSink>,
        config: Arc<ConfigStore>,
        resource_backend: ResourceBackend,
        observed_resource_backend: ResourceBackend,
        aggregator_projection_state: AggregatorProjectionState,
        host_name: HostName,
        canonical_local_host_id: Option<CanonicalHostId>,
        transport: Arc<dyn FleetReplicaTransport>,
    ) -> Self {
        let (fleet_replica_tx, _) = broadcast::channel(32);
        Self {
            _event_sink: event_sink,
            config,
            resource_backend,
            observed_resource_backend,
            aggregator_projection_state,
            host_name,
            canonical_local_host_id,
            transport,
            fleet_replica_cache: RwLock::new(HashMap::new()),
            fleet_replica_tx,
            resource_replication_failures: RwLock::new(HashMap::new()),
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Vec<FleetReplicaSnapshot>> {
        self.fleet_replica_tx.subscribe()
    }

    pub(crate) async fn cached_snapshots(&self) -> Vec<FleetReplicaSnapshot> {
        self.fleet_replica_cache
            .read()
            .await
            .iter()
            .map(|(host, entry)| FleetReplicaSnapshot {
                host: host.clone(),
                generation: entry.generation.clone(),
                rows: entry.rows.clone(),
                result_sets: entry.result_sets.clone(),
            })
            .collect()
    }

    pub(crate) async fn cached_rows_for_configured_hosts(&self) -> Vec<(HostName, Vec<FleetListRow>, Vec<ResultSet>)> {
        let hosts: HashSet<HostName> = self
            .config
            .load_hosts()
            .map(|hosts| {
                hosts
                    .hosts
                    .into_values()
                    .filter(|remote| !remote.agentless_ssh)
                    .map(|remote| HostName::new(remote.expected_host_name))
                    .collect()
            })
            .unwrap_or_default();
        let cache = self.fleet_replica_cache.read().await;
        hosts.into_iter().filter_map(|host| cache.get(&host).map(|entry| (host, entry.rows.clone(), entry.result_sets.clone()))).collect()
    }

    pub(crate) async fn replication_failures(&self) -> HashMap<NodeId, BTreeMap<String, String>> {
        self.resource_replication_failures.read().await.clone()
    }

    pub(crate) async fn begin_peer_resource_replication(&self, peer: &NodeId) {
        self.resource_replication_failures.write().await.remove(peer);
    }

    pub(crate) async fn report_resource_replication_failure(&self, peer: &NodeId, kind: &str, message: &str) {
        self.resource_replication_failures.write().await.entry(peer.clone()).or_default().insert(kind.to_string(), message.to_string());
    }

    pub(crate) async fn report_resource_replication_healthy(&self, peer: &NodeId, kind: &str) {
        let mut failures = self.resource_replication_failures.write().await;
        let Some(peer_failures) = failures.get_mut(peer) else { return };
        peer_failures.remove(kind);
        if peer_failures.is_empty() {
            failures.remove(peer);
        }
    }

    fn host_name_for_canonical_ref(&self, canonical_ref: &CanonicalHostId) -> HostName {
        if self.canonical_local_host_id.as_ref() == Some(canonical_ref) {
            self.host_name.clone()
        } else {
            HostName::new(canonical_ref.as_str())
        }
    }

    pub(crate) async fn refresh_once(&self, namespace: &str, runner: Option<Arc<dyn CommandRunner>>) -> Result<(), String> {
        let hosts = self.config.load_hosts()?;
        let runner = runner.ok_or_else(|| "local command runner unavailable".to_string())?;
        let configured: HashSet<_> = hosts
            .hosts
            .values()
            .filter(|remote| !remote.agentless_ssh)
            .map(|remote| HostName::new(remote.expected_host_name.clone()))
            .collect();
        {
            let mut cache = self.fleet_replica_cache.write().await;
            cache.retain(|host, _| configured.contains(host));
        }
        let fetches = hosts.hosts.iter().filter(|(_, remote)| !remote.agentless_ssh).map(|(label, remote)| {
            let host = HostName::new(remote.expected_host_name.clone());
            let multiplex = hosts.resolved_ssh_multiplex(label);
            let runner = Arc::clone(&runner);
            async move { (host, self.transport.fetch(remote, multiplex, runner).await) }
        });
        for (host, result) in join_all(fetches).await {
            match result {
                Ok(parsed) => {
                    let now = Utc::now();
                    let snapshot = parsed.snapshot;
                    let snapshot_host = snapshot.host;
                    let generation = snapshot.generation;
                    let result_sets = snapshot.result_sets.clone();
                    let staleness = FleetStaleness::Fresh { last_sync: now };
                    let mut rows: Vec<_> = snapshot
                        .rows
                        .into_iter()
                        .map(|mut row| {
                            row.host = snapshot_host.clone();
                            row.staleness = staleness.clone();
                            row
                        })
                        .collect();
                    // Replica rows from current daemons already include crewless rows via local_fleet_rows.
                    // Keep result-set rows as a secondary source for direct snapshots; existing rows win.
                    append_crewless_convoy_rows(&mut rows, namespace, &snapshot.result_sets, &snapshot_host, staleness);
                    self.fleet_replica_cache.write().await.insert(host, FleetReplicaCacheEntry {
                        rows,
                        result_sets,
                        last_sync: Some(now),
                        generation,
                        last_error: None,
                    });
                }
                Err(message) => {
                    let mut cache = self.fleet_replica_cache.write().await;
                    cache.entry(host).and_modify(|entry| entry.last_error = Some(message.clone())).or_insert_with(|| {
                        FleetReplicaCacheEntry {
                            rows: Vec::new(),
                            result_sets: Vec::new(),
                            last_sync: None,
                            generation: None,
                            last_error: Some(message),
                        }
                    });
                }
            }
        }
        if self.fleet_replica_tx.receiver_count() > 0 {
            let _ = self.fleet_replica_tx.send(self.cached_snapshots().await);
        }
        Ok(())
    }

    pub(crate) async fn rows(
        &self,
        namespace: &str,
        host_registry: &HostRegistry,
        source: FleetRowSource,
    ) -> Result<(Vec<FleetListRow>, Option<String>), String> {
        let now = Utc::now();
        let terminal_sessions = self.resource_backend.clone().using::<ResourceTerminalSession>(namespace);
        let environments = self.resource_backend.clone().using::<ResourceEnvironment>(namespace);
        let checkouts = self.resource_backend.clone().using::<ResourceCheckout>(namespace);
        let convoys = self.resource_backend.clone().using::<ResourceConvoy>(namespace);
        let observed_checkouts = self.observed_resource_backend.clone().using::<ResourceCheckout>(namespace);

        let session_list = if matches!(source, FleetRowSource::IncludingReplicas) {
            self.resource_backend
                .including_replicas::<ResourceTerminalSession>(namespace)
                .list()
                .await
                .map_err(|err| err.to_string())?
                .items
        } else {
            terminal_sessions
                .list()
                .await
                .map_err(|err| err.to_string())?
                .items
                .into_iter()
                .map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local })
                .collect()
        };
        let host_sources =
            self.resource_backend.including_replicas::<ResourceHost>(namespace).list().await.map_err(|err| err.to_string())?;
        let observed_generation = observed_checkouts.list().await.map_err(|err| err.to_string())?.generation;
        let result_sets = if matches!(source, FleetRowSource::IncludingReplicas) {
            vec![self.aggregator_projection_state.result_set().await]
        } else {
            self.aggregator_projection_state.local_result_sets().await
        };
        let placement_by_convoy = result_sets
            .iter()
            .filter_map(|result_set| result_set.rows.as_convoys())
            .flatten()
            .filter_map(|convoy| {
                convoy
                    .placement_decision
                    .clone()
                    .map(|decision| ((convoy.resource.namespace.clone(), convoy.resource.name.clone()), decision))
            })
            .collect::<HashMap<_, _>>();
        let surface_by_convoy = result_sets
            .iter()
            .filter_map(|result_set| result_set.rows.as_convoys())
            .flatten()
            .map(|convoy| ((convoy.resource.namespace.clone(), convoy.resource.name.clone()), convoy.surface_state))
            .collect::<HashMap<_, _>>();
        let environment_map: HashMap<_, _> = environments
            .list()
            .await
            .map_err(|err| err.to_string())?
            .items
            .into_iter()
            .map(|environment| (environment.metadata.name.clone(), environment))
            .collect();
        let convoy_items = if matches!(source, FleetRowSource::IncludingReplicas) {
            self.resource_backend
                .including_replicas::<ResourceConvoy>(namespace)
                .list()
                .await
                .map_err(|err| err.to_string())?
                .items
                .into_iter()
                .map(|item| item.object)
                .collect()
        } else {
            convoys.list().await.map_err(|err| err.to_string())?.items
        };
        let convoy_addresses = convoy_items
            .iter()
            .map(|convoy| {
                let address = convoy
                    .spec
                    .project_ref
                    .as_ref()
                    .map_or_else(|| convoy.spec.role.clone(), |project| format!("{} @ {project}", convoy.spec.role));
                (convoy.metadata.name.clone(), address)
            })
            .collect::<HashMap<_, _>>();
        let mut authority_by_convoy = HashMap::new();
        for checkout in checkouts.list().await.map_err(|err| err.to_string())?.items {
            let Some(convoy) = checkout.metadata.labels.get(CONVOY_LABEL).cloned() else {
                continue;
            };
            let authority = checkout
                .metadata
                .lifecycle_authority()
                .map_err(|err| err.to_string())?
                .map(|authority| authority.as_label_value().to_string());
            if authority.is_some() {
                authority_by_convoy.insert(convoy, authority);
            }
        }

        let mut rows = Vec::new();
        for session_source in session_list {
            let session = session_source.object;
            let remote_origin = match &session_source.provenance {
                ResourceProvenance::Local => None,
                ResourceProvenance::Replica { origin_root, last_synced_at } => Some((origin_root, *last_synced_at)),
            };
            let labels = &session.metadata.labels;
            let convoy = labels.get(CONVOY_LABEL).cloned().unwrap_or_else(|| "-".to_string());
            let task = labels.get(VESSEL_LABEL).cloned();
            let role = labels.get(ROLE_LABEL).cloned().unwrap_or_else(|| session.spec.role.clone());
            let crew = match task.as_ref() {
                Some(task) => format!("{task}/{role}"),
                None => role.clone(),
            };
            let attention = crew_attention(session.status.as_ref(), now);
            let convoy_key = (session.metadata.namespace.clone(), convoy.clone());
            let host = if let Some((origin_root, _)) = remote_origin {
                host_registry.host_name_for_node(origin_root).await.unwrap_or_else(|| HostName::new(origin_root.as_str()))
            } else if let Some(host_ref) =
                environment_map.get(&session.spec.env_ref).and_then(|environment| resource_environment_host_ref(environment))
            {
                canonical_placement_host_ref_from_sources(&host_sources.items, host_ref).ok().flatten().map_or_else(
                    || {
                        if self.canonical_local_host_id.clone().is_some_and(|local| local.as_str() == host_ref) {
                            self.host_name.clone()
                        } else {
                            HostName::new(host_ref)
                        }
                    },
                    |target| self.host_name_for_canonical_ref(&target.reference),
                )
            } else {
                self.host_name.clone()
            };
            rows.push(
                FleetListRow::builder()
                    .convoy(convoy_addresses.get(&convoy).cloned().unwrap_or_else(|| convoy.clone()))
                    .maybe_convoy_ref((convoy != "-").then_some(convoy.clone()))
                    .vessel(session.spec.env_ref.clone())
                    .maybe_authority(authority_by_convoy.get(&convoy).cloned().flatten())
                    .crew(crew)
                    .crew_state(session_status_label(session.status.as_ref().map(|status| status.phase)))
                    .surface_state(surface_by_convoy.get(&convoy_key).copied().unwrap_or_default())
                    .maybe_attention(attention)
                    .host(host)
                    .maybe_placement_decision(placement_by_convoy.get(&convoy_key).cloned())
                    .namespace(session.metadata.namespace.clone())
                    .session(session.metadata.name.clone())
                    .staleness(match remote_origin {
                        Some((_, last_sync)) if !replica_sync_is_fresh(last_sync, now) => FleetStaleness::Stale { last_sync },
                        Some((_, last_sync)) => FleetStaleness::Fresh { last_sync },
                        None => FleetStaleness::Local,
                    })
                    .build(),
            );
        }
        append_crewless_convoy_rows(&mut rows, namespace, &result_sets, &self.host_name, FleetStaleness::Local);
        if matches!(source, FleetRowSource::IncludingReplicas) {
            let mut sync_by_host = HashMap::new();
            for source in &host_sources.items {
                let ResourceProvenance::Replica { origin_root, last_synced_at } = &source.provenance else {
                    continue;
                };
                if !is_host_self_report(source, host_registry).await {
                    continue;
                }
                if let Some(host) = host_registry.host_name_for_node(origin_root).await {
                    sync_by_host.insert(host, *last_synced_at);
                }
            }
            for row in &mut rows {
                if row.host == self.host_name || !matches!(row.staleness, FleetStaleness::Local) {
                    continue;
                }
                if let Some(last_sync) = sync_by_host.get(&row.host).copied() {
                    row.staleness = if !replica_sync_is_fresh(last_sync, now) {
                        FleetStaleness::Stale { last_sync }
                    } else {
                        FleetStaleness::Fresh { last_sync }
                    };
                }
            }
        }
        rows.sort_by(|left, right| {
            (&left.convoy, left.host.as_str(), &left.vessel, &left.crew).cmp(&(
                &right.convoy,
                right.host.as_str(),
                &right.vessel,
                &right.crew,
            ))
        });
        Ok((rows, observed_generation))
    }
}

fn session_status_label(phase: Option<ResourceTerminalSessionPhase>) -> String {
    match phase {
        Some(ResourceTerminalSessionPhase::Starting) | None => "starting".to_string(),
        Some(ResourceTerminalSessionPhase::Running) => "running".to_string(),
        Some(ResourceTerminalSessionPhase::Lost) => "lost".to_string(),
        Some(ResourceTerminalSessionPhase::Stopped) => "stopped".to_string(),
        Some(ResourceTerminalSessionPhase::Failed) => "failed".to_string(),
    }
}

pub(crate) fn crew_attention(status: Option<&TerminalSessionStatus>, now: DateTime<Utc>) -> Option<CrewAttention> {
    let status = status.filter(|status| status.phase == ResourceTerminalSessionPhase::Running)?;
    if status.degraded.as_ref().is_some_and(|condition| condition.reason == "DeliveryUnconfirmed") {
        return Some(CrewAttention::DeliveryUnconfirmed);
    }
    let attention = status.attention.as_ref()?;
    if attention.is_stale_at(now) {
        return Some(CrewAttention::Unobservable);
    }
    Some(match attention.state {
        TerminalAttentionState::Working => CrewAttention::Working,
        TerminalAttentionState::NeedsInput => CrewAttention::NeedsInput,
        TerminalAttentionState::Idle => CrewAttention::Idle,
        TerminalAttentionState::Unobservable => CrewAttention::Unobservable,
    })
}

fn ssh_destination(remote: &RemoteHostConfig) -> String {
    crate::config::ssh_destination(&remote.hostname, remote.user.as_deref())
}

fn fleet_replica_ssh_args(remote: &RemoteHostConfig, multiplex: bool) -> Vec<String> {
    let mut args = vec![
        "-T".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={}", FLEET_REPLICA_REFRESH_TIMEOUT.as_secs()),
        "-o".to_string(),
        "ConnectionAttempts=1".to_string(),
    ];
    if multiplex {
        args.extend([
            "-o".to_string(),
            "ControlMaster=auto".to_string(),
            "-o".to_string(),
            "ControlPath=/tmp/flotilla-ssh-%C".to_string(),
            "-o".to_string(),
            "ControlPersist=60".to_string(),
        ]);
    }
    args.push(ssh_destination(remote));
    let snapshot_command = vec![
        Arg::Literal("cd".to_string()),
        Arg::Quoted("/".to_string()),
        Arg::Literal("&&".to_string()),
        Arg::Literal("exec".to_string()),
        Arg::Literal("flotilla".to_string()),
        Arg::Literal("--json".to_string()),
        Arg::Quoted("replica-snapshot".to_string()),
    ];
    let login_wrapper = vec![
        Arg::Literal("${SHELL:-/bin/sh}".to_string()),
        Arg::Literal("-l".to_string()),
        Arg::Literal("-c".to_string()),
        Arg::NestedCommand(snapshot_command),
    ];
    args.push(flatten(&login_wrapper, 0));
    args
}

#[cfg(test)]
pub(crate) fn replica_staleness(entry: &FleetReplicaCacheEntry, now: DateTime<Utc>) -> FleetStaleness {
    if let Some(message) = &entry.last_error {
        return FleetStaleness::Unreachable { last_sync: entry.last_sync, message: message.clone() };
    }
    let Some(last_sync) = entry.last_sync else {
        return FleetStaleness::Unreachable { last_sync: None, message: "replica has never synced".to_string() };
    };
    if !replica_sync_is_fresh(last_sync, now) {
        FleetStaleness::Stale { last_sync }
    } else {
        FleetStaleness::Fresh { last_sync }
    }
}

pub(crate) fn accumulate_fleet_health_counts(counts: &mut HashMap<HostName, (usize, HashSet<String>)>, rows: &[FleetListRow]) {
    for row in rows {
        let (crew_count, convoys) = counts.entry(row.host.clone()).or_default();
        if row.crew != "-" {
            *crew_count += 1;
        }
        if row.convoy != "-" {
            convoys.insert(row.convoy.clone());
        }
    }
}

/// One attention entry per expired or near-expiry credential scope on a host,
/// derived from the `credential_expiry` capability its heartbeat publishes.
pub(crate) fn host_credential_attention(
    status: &ResourceHostStatus,
    now: DateTime<Utc>,
    warning_window: chrono::Duration,
) -> Vec<CredentialAttention> {
    let Ok(expiry) = status.credential_expiry() else {
        return vec![CredentialAttention {
            severity: CredentialAttentionSeverity::Unreadable,
            message: "credential expiry capability is unreadable".to_string(),
        }];
    };
    let mut attention = Vec::new();
    for (scope, entry) in expiry {
        let label = if scope == flotilla_resources::AMBIENT_CLAUDE_CREDENTIAL_SCOPE {
            "ambient claude login".to_string()
        } else {
            format!("credential `{scope}`")
        };
        if let Some(expired_at) = entry.expired_at(now) {
            attention.push(CredentialAttention {
                severity: CredentialAttentionSeverity::Expired,
                message: format!("{label} expired on {}", expired_at.format("%Y-%m-%d")),
            });
        } else if let Some(expires_at) = entry.expires_within(now, warning_window) {
            attention.push(CredentialAttention {
                severity: CredentialAttentionSeverity::Expiring,
                message: format!("{label} expires on {}", expires_at.format("%Y-%m-%d")),
            });
        }
    }
    attention
}

pub(crate) fn fleet_observation_agreement(
    link: &PeerConnectionState,
    heartbeat_at: Option<DateTime<Utc>>,
    heartbeat_fresh: bool,
    daemon_generation: Option<&str>,
    is_local: bool,
) -> FleetObservationAgreement {
    if is_local {
        return FleetObservationAgreement::Agree;
    }
    let link_disagrees = match link {
        PeerConnectionState::Connected => !heartbeat_fresh,
        PeerConnectionState::Disconnected | PeerConnectionState::Rejected { .. } => heartbeat_fresh,
        PeerConnectionState::Connecting | PeerConnectionState::Reconnecting => false,
    };
    if link_disagrees {
        FleetObservationAgreement::Disagree
    } else if heartbeat_at.is_none()
        || matches!(link, PeerConnectionState::Connecting | PeerConnectionState::Reconnecting)
        || daemon_generation.is_none()
    {
        FleetObservationAgreement::Unknown
    } else {
        FleetObservationAgreement::Agree
    }
}

pub(crate) fn format_resource_replication_failures(failures: &[ResourceReplicationFailure]) -> Option<String> {
    if failures.is_empty() {
        return None;
    }
    Some(format!(
        "resource replication failed: {}",
        failures.iter().map(|failure| format!("{}: {}", failure.kind, failure.message)).collect::<Vec<_>>().join("; ")
    ))
}

pub(crate) fn join_replica_errors(first: Option<&str>, second: Option<&str>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) => Some(format!("{first}; {second}")),
        (Some(message), None) | (None, Some(message)) => Some(message.to_string()),
        (None, None) => None,
    }
}

#[derive(Debug, Default)]
struct ReplicaParseDiagnostics {
    skipped_records: usize,
    first_error: Option<String>,
}

impl ReplicaParseDiagnostics {
    fn record_skip(&mut self, path: impl std::fmt::Display, error: impl std::fmt::Display) {
        self.record_skips(1, path, error);
    }

    fn record_skips(&mut self, count: usize, path: impl std::fmt::Display, error: impl std::fmt::Display) {
        self.skipped_records += count;
        self.first_error.get_or_insert_with(|| format!("{path}: {error}"));
    }
}

#[derive(Debug)]
pub(crate) struct ParsedFleetReplicaSnapshot {
    snapshot: FleetReplicaSnapshot,
    #[cfg_attr(not(test), allow(dead_code))]
    diagnostics: ReplicaParseDiagnostics,
}

fn result_set_records_mut(result_set: &mut serde_json::Value) -> Option<&mut Vec<serde_json::Value>> {
    result_set.get_mut("rows")?.get_mut("rows")?.get_mut("rows")?.as_array_mut()
}

fn retain_parseable_result_set_records(
    result_set: &mut serde_json::Value,
    result_set_index: usize,
    diagnostics: &mut ReplicaParseDiagnostics,
) -> bool {
    let Some(records) = result_set_records_mut(result_set) else {
        return match serde_json::from_value::<ResultSet>(result_set.clone()) {
            Ok(_) => true,
            Err(error) => {
                diagnostics.record_skip(format_args!("result_sets[{result_set_index}]"), error);
                false
            }
        };
    };
    let records = std::mem::take(records);

    let envelope = result_set.clone();
    if let Err(error) = serde_json::from_value::<ResultSet>(envelope.clone()) {
        diagnostics.record_skips(records.len().max(1), format_args!("result_sets[{result_set_index}]"), error);
        return false;
    }

    let mut retained = Vec::with_capacity(records.len());
    for (record_index, record) in records.into_iter().enumerate() {
        let mut candidate = envelope.clone();
        result_set_records_mut(&mut candidate).expect("validated result set has a row array").push(record.clone());
        match serde_json::from_value::<ResultSet>(candidate) {
            Ok(_) => retained.push(record),
            Err(error) => {
                diagnostics.record_skip(format_args!("result_sets[{result_set_index}].rows[{record_index}]"), error);
            }
        }
    }
    *result_set_records_mut(result_set).expect("validated result set has a row array") = retained;
    true
}

fn parse_fleet_replica_snapshot(input: &str) -> Result<ParsedFleetReplicaSnapshot, String> {
    let mut value: serde_json::Value = serde_json::from_str(input).map_err(|error| format!("replica snapshot parse failed: {error}"))?;
    let mut diagnostics = ReplicaParseDiagnostics::default();

    if let Some(rows) = value.get_mut("rows").and_then(serde_json::Value::as_array_mut) {
        let records = std::mem::take(rows);
        for (index, record) in records.into_iter().enumerate() {
            match serde_json::from_value::<FleetListRow>(record.clone()) {
                Ok(_) => rows.push(record),
                Err(error) => diagnostics.record_skip(format_args!("rows[{index}]"), error),
            }
        }
    }

    if let Some(result_sets) = value.get_mut("result_sets").and_then(serde_json::Value::as_array_mut) {
        let records = std::mem::take(result_sets);
        for (index, mut record) in records.into_iter().enumerate() {
            if retain_parseable_result_set_records(&mut record, index, &mut diagnostics) {
                result_sets.push(record);
            }
        }
    }

    let snapshot = serde_json::from_value(value).map_err(|error| format!("replica snapshot parse failed outside a record: {error}"))?;
    Ok(ParsedFleetReplicaSnapshot { snapshot, diagnostics })
}

#[derive(Debug, Clone)]
pub(crate) struct FleetReplicaCacheEntry {
    pub(crate) rows: Vec<FleetListRow>,
    pub(crate) result_sets: Vec<ResultSet>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) last_sync: Option<DateTime<Utc>>,
    pub(crate) generation: Option<String>,
    pub(crate) last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ResourceReplicationFailure {
    pub(crate) kind: String,
    pub(crate) message: String,
}

fn convoy_state_label(row: &ConvoyRow) -> String {
    match row.message.as_deref().filter(|message| !message.trim().is_empty()) {
        Some(message) => format!("{}: {message}", row.phase),
        None => row.phase.to_string(),
    }
}

fn append_crewless_convoy_rows(
    rows: &mut Vec<FleetListRow>,
    target_namespace: &str,
    result_sets: &[ResultSet],
    host: &HostName,
    staleness: FleetStaleness,
) {
    let mut convoys_with_crew: HashSet<String> = rows.iter().filter_map(|row| row.convoy_ref.clone()).collect();
    for result_set in result_sets {
        let Rows::Convoys { rows: convoys, .. } = &result_set.rows else { continue };
        for row in convoys {
            if row.resource.namespace != target_namespace {
                continue;
            }
            if !convoys_with_crew.insert(row.resource.name.clone()) {
                continue;
            }
            let display = row.project_ref.as_ref().map_or_else(|| row.name.clone(), |project| format!("{} @ {project}", row.name));
            rows.push(
                FleetListRow::builder()
                    .convoy(display)
                    .convoy_ref(row.resource.name.clone())
                    .vessel("-")
                    .crew("-")
                    .crew_state(convoy_state_label(row))
                    .surface_state(row.surface_state)
                    .host(row.resource.host.clone().unwrap_or_else(|| host.clone()))
                    .maybe_placement_decision(row.placement_decision.clone())
                    .namespace(target_namespace)
                    .staleness(staleness.clone())
                    .build(),
            );
        }
    }
}

fn resource_environment_host_ref(environment: &flotilla_resources::ResourceObject<ResourceEnvironment>) -> Option<&str> {
    environment
        .spec
        .host_direct
        .as_ref()
        .map(|spec| spec.host_ref.as_str())
        .or_else(|| environment.spec.docker.as_ref().map(|spec| spec.host_ref.as_str()))
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use flotilla_protocol::FleetReplicaSnapshot;
    use flotilla_resources::{TerminalAttention, TerminalAttentionSource};
    use tokio::sync::Barrier;

    use super::*;
    use crate::providers::ProcessCommandRunner;

    #[test]
    fn crew_attention_keeps_monitoring_distinct_from_lifecycle_state() {
        let now = Utc::now();
        let mut status = TerminalSessionStatus {
            phase: ResourceTerminalSessionPhase::Running,
            attention: Some(TerminalAttention { state: TerminalAttentionState::Idle, as_of: now, source: TerminalAttentionSource::Screen }),
            ..Default::default()
        };

        assert_eq!(crew_attention(Some(&status), now), Some(CrewAttention::Idle));

        status.degraded = Some(flotilla_resources::TerminalSessionDegradedCondition {
            reason: "DeliveryUnconfirmed".to_string(),
            message: "composer retained delivery".to_string(),
            message_id: Some("handoff-1".to_string()),
            consecutive_failures: 1,
            observed_at: now,
        });
        assert_eq!(crew_attention(Some(&status), now), Some(CrewAttention::DeliveryUnconfirmed));
        status.degraded = None;

        status.attention.as_mut().expect("attention").as_of = now - TerminalAttention::FRESH_FOR;
        assert_eq!(crew_attention(Some(&status), now), Some(CrewAttention::Unobservable));

        status.phase = ResourceTerminalSessionPhase::Stopped;
        assert_eq!(crew_attention(Some(&status), now), None);
    }

    fn snapshot() -> FleetReplicaSnapshot {
        FleetReplicaSnapshot {
            host: HostName::new("remote"),
            generation: Some("g1".into()),
            rows: vec![FleetListRow::builder()
                .convoy("convoy")
                .vessel("vessel")
                .crew("crew")
                .crew_state("running")
                .host(HostName::new("untrusted-row-host"))
                .namespace("flotilla")
                .staleness(FleetStaleness::Local)
                .build()],
            result_sets: vec![],
        }
    }

    #[test]
    fn parse_skips_bad_rows_without_discarding_snapshot() {
        let mut value = serde_json::to_value(snapshot()).expect("serialize snapshot");
        value["rows"] = serde_json::json!([{}, {"broken": true}]);
        let parsed = parse_fleet_replica_snapshot(&value.to_string()).expect("parse snapshot");
        assert!(parsed.snapshot.rows.is_empty());
        assert_eq!(parsed.diagnostics.skipped_records, 2);
        assert!(parsed.diagnostics.first_error.expect("diagnostic").starts_with("rows[0]:"));
    }

    #[test]
    fn agreement_distinguishes_disagreement_from_missing_evidence() {
        let now = Utc::now();
        assert_eq!(
            fleet_observation_agreement(&PeerConnectionState::Connected, Some(now), true, Some("a"), false),
            FleetObservationAgreement::Agree
        );
        assert_eq!(
            fleet_observation_agreement(&PeerConnectionState::Disconnected, Some(now), true, Some("a"), false),
            FleetObservationAgreement::Disagree
        );
        assert_eq!(
            fleet_observation_agreement(&PeerConnectionState::Connecting, Some(now), true, Some("a"), false),
            FleetObservationAgreement::Unknown
        );
        assert_eq!(
            fleet_observation_agreement(&PeerConnectionState::Disconnected, Some(now), true, Some("a"), true),
            FleetObservationAgreement::Agree
        );
    }

    #[test]
    fn staleness_keeps_last_success_when_refresh_fails() {
        let now = Utc::now();
        let mut entry =
            FleetReplicaCacheEntry { rows: vec![], result_sets: vec![], last_sync: Some(now), generation: None, last_error: None };
        assert!(matches!(replica_staleness(&entry, now), FleetStaleness::Fresh { .. }));
        entry.last_error = Some("ssh failed".into());
        assert!(matches!(replica_staleness(&entry, now), FleetStaleness::Unreachable { last_sync: Some(_), .. }));
    }

    struct FakeTransport(Mutex<VecDeque<Result<ParsedFleetReplicaSnapshot, String>>>);

    #[async_trait]
    impl FleetReplicaTransport for FakeTransport {
        async fn fetch(&self, _: &RemoteHostConfig, _: bool, _: Arc<dyn CommandRunner>) -> Result<ParsedFleetReplicaSnapshot, String> {
            self.0.lock().expect("transport queue").pop_front().expect("queued result")
        }
    }

    struct RendezvousTransport(Barrier);

    #[async_trait]
    impl FleetReplicaTransport for RendezvousTransport {
        async fn fetch(&self, remote: &RemoteHostConfig, _: bool, _: Arc<dyn CommandRunner>) -> Result<ParsedFleetReplicaSnapshot, String> {
            self.0.wait().await;
            if remote.expected_host_name == "failed" {
                Err("remote unavailable".into())
            } else {
                let mut snapshot = snapshot();
                snapshot.host = HostName::new("healthy");
                Ok(ParsedFleetReplicaSnapshot { snapshot, diagnostics: ReplicaParseDiagnostics::default() })
            }
        }
    }

    #[tokio::test]
    async fn refresh_fetches_hosts_concurrently_and_reports_each_result() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("hosts.toml"),
            "[hosts.healthy]\nhostname = 'healthy.example'\n[hosts.failed]\nhostname = 'failed.example'\n",
        )
        .expect("host config");
        let service = FleetService::new(
            Arc::new(crate::event_sink::RecordingEventSink::default()),
            Arc::new(ConfigStore::with_base(temp.path())),
            ResourceBackend::InMemory(flotilla_resources::InMemoryBackend::default()),
            ResourceBackend::InMemory(flotilla_resources::InMemoryBackend::observed()),
            AggregatorProjectionState::new(),
            HostName::new("local"),
            None,
            Arc::new(RendezvousTransport(Barrier::new(2))),
        );
        let mut subscriber = service.subscribe();
        let runner: Arc<dyn CommandRunner> = Arc::new(ProcessCommandRunner);
        tokio::time::timeout(Duration::from_secs(1), service.refresh_once("flotilla", Some(runner)))
            .await
            .expect("both remote fetches must start together")
            .expect("refresh cycle");
        let mut snapshots = subscriber.recv().await.expect("broadcast after refresh");
        snapshots.sort_by(|left, right| left.host.cmp(&right.host));
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].host, HostName::new("failed"));
        assert!(snapshots[0].rows.is_empty());
        assert_eq!(snapshots[1].host, HostName::new("healthy"));
        assert_eq!(snapshots[1].generation.as_deref(), Some("g1"));
        let error = service.fleet_replica_cache.read().await.get(&HostName::new("failed")).and_then(|entry| entry.last_error.clone());
        assert_eq!(error.as_deref(), Some("remote unavailable"));
    }

    #[tokio::test]
    async fn refresh_retains_last_snapshot_and_broadcasts_failed_refresh() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("hosts.toml"), "[hosts.remote]\nhostname = 'remote.example'\n").expect("host config");
        let service = FleetService::new(
            Arc::new(crate::event_sink::RecordingEventSink::default()),
            Arc::new(ConfigStore::with_base(temp.path())),
            ResourceBackend::InMemory(flotilla_resources::InMemoryBackend::default()),
            ResourceBackend::InMemory(flotilla_resources::InMemoryBackend::observed()),
            AggregatorProjectionState::new(),
            HostName::new("local"),
            None,
            Arc::new(FakeTransport(Mutex::new(VecDeque::from([
                Ok(ParsedFleetReplicaSnapshot { snapshot: snapshot(), diagnostics: ReplicaParseDiagnostics::default() }),
                Err("ssh failed".into()),
            ])))),
        );
        let mut subscriber = service.subscribe();
        let runner: Arc<dyn CommandRunner> = Arc::new(ProcessCommandRunner);
        service.refresh_once("flotilla", Some(Arc::clone(&runner))).await.expect("first refresh");
        assert_eq!(subscriber.recv().await.expect("first broadcast")[0].generation.as_deref(), Some("g1"));
        service.refresh_once("flotilla", Some(runner)).await.expect("failed remote fetch does not fail cycle");
        assert_eq!(subscriber.recv().await.expect("second broadcast")[0].generation.as_deref(), Some("g1"));
        let cache = service.fleet_replica_cache.read().await;
        let entry = cache.get(&HostName::new("remote")).expect("cached host");
        assert_eq!(entry.last_error.as_deref(), Some("ssh failed"));
        assert_eq!(entry.generation.as_deref(), Some("g1"));
        assert_eq!(entry.rows.len(), 1);
        assert_eq!(entry.rows[0].host, HostName::new("remote"));
        assert!(matches!(entry.rows[0].staleness, FleetStaleness::Fresh { .. }));
    }
}
