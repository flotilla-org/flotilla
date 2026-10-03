//! Fleet views and health evidence from replicated resources.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};
use flotilla_protocol::{
    result_set::{ConvoyRow, ResultSet, Rows},
    CanonicalHostId, CredentialAttention, CredentialAttentionSeverity, CrewAttention, FleetListRow, FleetObservationAgreement,
    FleetStaleness, HostName, NodeId, PeerConnectionState,
};
use flotilla_resources::{
    Checkout as ResourceCheckout, Convoy as ResourceConvoy, Environment as ResourceEnvironment, Host as ResourceHost,
    HostStatus as ResourceHostStatus, ReadResourceObject, ResourceBackend, ResourceProvenance, TerminalAttentionState,
    TerminalSession as ResourceTerminalSession, TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionStatus, CONVOY_LABEL,
    ROLE_LABEL, VESSEL_LABEL,
};
use tokio::sync::RwLock;
use tracing::debug;

use crate::{
    aggregator_projection::AggregatorProjectionState, host_registry::HostRegistry,
    host_resolution::canonical_placement_host_ref_from_sources,
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

pub(crate) struct ReplicatedHostReport {
    pub(crate) host_id: String,
    pub(crate) last_synced_at: DateTime<Utc>,
    pub(crate) status: Option<ResourceHostStatus>,
}

pub(crate) async fn replicated_host_reports(
    sources: &[ReadResourceObject<ResourceHost>],
    host_registry: &HostRegistry,
    configured_by_node: &HashMap<NodeId, HostName>,
) -> HashMap<HostName, ReplicatedHostReport> {
    let mut reports = HashMap::<HostName, ReplicatedHostReport>::new();
    for source in sources {
        let ResourceProvenance::Replica { origin_root, last_synced_at } = &source.provenance else { continue };
        if !is_host_self_report(source, host_registry).await {
            continue;
        }
        let Some(host) = host_registry.host_name_for_node(origin_root).await.or_else(|| configured_by_node.get(origin_root).cloned())
        else {
            continue;
        };
        if reports.get(&host).is_some_and(|existing| {
            existing.last_synced_at > *last_synced_at
                || (existing.last_synced_at == *last_synced_at && existing.host_id <= source.object.metadata.name)
        }) {
            continue;
        }
        reports.insert(host, ReplicatedHostReport {
            host_id: source.object.metadata.name.clone(),
            last_synced_at: *last_synced_at,
            status: source.object.status.clone(),
        });
    }
    reports
}
pub(crate) struct FleetService {
    resource_backend: ResourceBackend,
    aggregator_projection_state: AggregatorProjectionState,
    host_name: HostName,
    canonical_local_host_id: Option<CanonicalHostId>,
    resource_replication_failures: RwLock<HashMap<NodeId, BTreeMap<String, String>>>,
}

impl FleetService {
    pub(crate) fn new(
        resource_backend: ResourceBackend,
        aggregator_projection_state: AggregatorProjectionState,
        host_name: HostName,
        canonical_local_host_id: Option<CanonicalHostId>,
    ) -> Self {
        Self {
            resource_backend,
            aggregator_projection_state,
            host_name,
            canonical_local_host_id,
            resource_replication_failures: RwLock::new(HashMap::new()),
        }
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

    pub(crate) async fn rows(&self, namespace: &str, host_registry: &HostRegistry) -> Result<Vec<FleetListRow>, String> {
        let now = Utc::now();
        let environments = self.resource_backend.clone().using::<ResourceEnvironment>(namespace);
        let checkouts = self.resource_backend.clone().using::<ResourceCheckout>(namespace);

        let session_list = self
            .resource_backend
            .including_replicas::<ResourceTerminalSession>(namespace)
            .list()
            .await
            .map_err(|err| err.to_string())?
            .items;
        let host_sources =
            self.resource_backend.including_replicas::<ResourceHost>(namespace).list().await.map_err(|err| err.to_string())?;
        let result_sets = vec![self.aggregator_projection_state.result_set().await];
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
        let convoy_items = self
            .resource_backend
            .including_replicas::<ResourceConvoy>(namespace)
            .list()
            .await
            .map_err(|err| err.to_string())?
            .items
            .into_iter()
            .map(|item| item.object)
            .collect::<Vec<_>>();
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
                // The origin may arrive before its host summary. Wait for the mapping instead of exposing a phantom host.
                let Some(host) = host_registry.host_name_for_node(origin_root).await else {
                    debug!(origin = %origin_root, session = %session.metadata.name, "omitting fleet row until host origin is mapped");
                    continue;
                };
                host
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
        let subjects_by_convoy = result_sets
            .iter()
            .filter_map(|result_set| result_set.rows.as_convoys())
            .flatten()
            .map(|convoy| ((convoy.resource.namespace.clone(), convoy.resource.name.clone()), convoy.subjects.clone()))
            .collect::<HashMap<_, _>>();
        for row in &mut rows {
            if let Some(convoy_ref) = &row.convoy_ref {
                row.subjects = subjects_by_convoy.get(&(row.namespace.clone(), convoy_ref.clone())).cloned().unwrap_or_default();
            }
        }
        {
            let reports = replicated_host_reports(&host_sources.items, host_registry, &HashMap::new()).await;
            for row in &mut rows {
                if row.host == self.host_name || !matches!(row.staleness, FleetStaleness::Local) {
                    continue;
                }
                if let Some(last_sync) = reports.get(&row.host).map(|report| report.last_synced_at) {
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
        Ok(rows)
    }
}

pub(crate) fn session_status_label(phase: Option<ResourceTerminalSessionPhase>) -> String {
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
    use flotilla_protocol::{ConvoyPhase, ResourceRef};
    use flotilla_resources::{TerminalAttention, TerminalAttentionSource};

    use super::*;

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

    #[test]
    fn crewless_convoy_rows_preserve_resource_host() {
        let local = HostName::new("local");
        let remote = HostName::new("remote");
        let convoy = ConvoyRow::builder()
            .resource(ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "convoy").on_host(remote.clone()))
            .name("convoy".to_string())
            .workflow_ref("workflow".to_string())
            .phase(ConvoyPhase::Active)
            .build();
        let result_sets = vec![ResultSet { seq: 1, rows: Rows::Convoys { scope: None, rows: vec![convoy] }, state: Default::default() }];

        let mut merged_rows = Vec::new();
        append_crewless_convoy_rows(&mut merged_rows, "flotilla", &result_sets, &local, FleetStaleness::Local);
        assert_eq!(merged_rows.len(), 1);
        assert_eq!(merged_rows[0].host, remote);
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
}
