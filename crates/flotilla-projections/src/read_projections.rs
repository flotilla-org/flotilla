//! Read-side query projections over resource state and explicit runtime inputs.

use crate::ports::EnvironmentSummaries;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_change_request::observation::ChangeRequestKey;
use flotilla_charter::ops_entry::{
    DECLARATION_REFUSAL_ATTENTION_PREFIX, DECLARATION_REFUSAL_REASON_ANNOTATION, DECLARATION_REFUSED_SINCE_ANNOTATION,
    DECLARATION_STALE_AFTER, ENSURE_CONFIG_DRIFT_REASON_ANNOTATION, ENSURE_DRIFT_ATTENTION_PREFIX,
};
use flotilla_config::ConfigStore;
use flotilla_manifest::{
    keys::{
        KEY_CHANGE_REQUEST_CHECKS, KEY_CHANGE_REQUEST_CHECKS_OBSERVED_AT, KEY_CHANGE_REQUEST_READINESS,
        KEY_CHANGE_REQUEST_REVIEW_ACTIONABLE_AT_HEAD, KEY_CHANGE_REQUEST_REVIEW_ACTIONABLE_AT_HEAD_OBSERVED_AT,
        KEY_CHANGE_REQUEST_REVIEW_DECISION, KEY_CHANGE_REQUEST_REVIEW_DECISION_OBSERVED_AT, KEY_CHANGE_REQUEST_STATE,
        KEY_CHANGE_REQUEST_STATE_OBSERVED_AT,
    },
    projection::{change_request_facts, evaluate_change_request_readiness},
    wire::MetadataValue,
};
use flotilla_protocol::{
    commands::{ExplainedSubjectFact, ExplainedSubjectObservation},
    ConvoyExplanation, DeclarationAttentionKind, DeclarationAttentionRow, DispatchQueueResponse, DispatchQueueRow, EnvironmentId,
    ExplainedArtifact, ExplainedChangeRequest, ExplainedCheckout, ExplainedCrewDelivery, ExplainedDecisionLedger, ExplainedEvent,
    ExplainedLeafFiring, ExplainedSettlement, ExplainedSubscription, ExplainedUnclaimedWork, ExplainedUnmetExpectation,
    FleetHealthResponse, FleetHostRow, FleetHostStaleness, FleetListResponse, FleetListRow, FleetReplicaStatus, FleetStaleness,
    FulfilmentHarness, FulfilmentListResponse, FulfilmentModel, FulfilmentRow, HostListResponse, HostName, HostProvidersResponse,
    HostStatusResponse, HostSummary, NodeId, PeerConnectionState, ProjectListEntry, ProjectListRepository, ProjectListResponse,
    ResourceRef, Subject, SubjectKind, ViewAddress,
};
use flotilla_provider_runtime::ObservationError;
use flotilla_resources::resolve_project_issue_sources_from_inventory;
use flotilla_resources::{
    address::{leaf_address, leaf_record_name, record_name},
    bound_change_request_address, convoy_subject_rows, evaluate_landing_settlement, expected_change_request_leaves, expected_checkout_refs,
    repository_display_labels, ChangeRequestStatus, Checkout as ResourceCheckout, Clock, ConditionValue, Convoy as ResourceConvoy,
    ConvoyStatus, CrewMessageSender, CrewWorkPhase, Demand as ResourceDemand, DemandState, Forge, FulfilmentKind, FulfilmentRealisation,
    Host as ResourceHost, HostStatus as ResourceHostStatus, IssueSourceResolution, IssueSourceUnavailable, ManifestRoot, Project,
    ReadResourceObject, Repository, RepositoryKey, ResourceObject, ResourceProvenance, SettlementMode, TerminalAttentionState,
    TerminalSession as ResourceTerminalSession, TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionSource, Vessel,
    WorkPhase as ResourceWorkPhase, WorkflowTemplate, CONVOY_LABEL, HEARTBEAT_READY_TTL_SECS, PROJECT_LABEL, ROLE_LABEL, VESSEL_LABEL,
};
use flotilla_store::{EventRecorder, ProjectHierarchyStoreExt, ResourceBackend, StoreQuery};
use tracing::warn;

use crate::resource_explain::{explain_condition, explained_provenance, observed_freshness};
use crate::{
    fleet::{
        accumulate_fleet_health_counts, fleet_observation_agreement, format_resource_replication_failures, host_credential_attention,
        join_replica_errors, message_controller_failure, replica_sync_is_fresh, replicated_host_reports, FleetService,
        ResourceReplicationFailure,
    },
    host_registry::HostCounts,
};
use flotilla_resources::LANDING_EVIDENCE_TTL;
use flotilla_resources::{
    explain_unmet_expectation, resolve_convoy_candidate_indices, ConvoyAddressIdentity, LeafFiringRecord, LeafSubscriptionRow, LeafWatcher,
};
use flotilla_store as project_ops;

/// Read projections can inspect subscription evidence without changing subscriptions or delivery.
#[async_trait]
pub trait LeafSubscriptionRead: Send + Sync {
    fn change_request_stale_after(&self) -> Duration;
    async fn change_request_observation_error(&self, subject: &ChangeRequestKey) -> Option<ObservationError>;
    async fn diagnostics(&self) -> Vec<(LeafSubscriptionRow, Vec<LeafFiringRecord>)>;
}

/// Immutable receiver evidence gathered from one request snapshot.
pub trait MessageReceiverRead: Send + Sync {
    fn terminals(&self) -> &[ReadResourceObject<ResourceTerminalSession>];
    fn may_address_convoy(&self, address: &str, project: &str, convoy: &str) -> bool;
    fn resolve(&self, address: &str) -> Result<Option<ReadResourceObject<ResourceTerminalSession>>, String>;
    /// The Messages in the old shape when they live on the engine (#3317):
    /// the local replica's, every home's. None reads the old store.
    fn messages(&self) -> Option<&[ReadResourceObject<flotilla_resources::Message>]> {
        None
    }
}

/// Resolve receiver evidence through the messaging resolver at the composition boundary.
/// The supplied backend is the request snapshot: implementations must read only from it.
#[async_trait]
pub trait MessageReceiverLoader: Send + Sync {
    async fn load(
        &self,
        backend: &ResourceBackend,
        namespace: &str,
        convoys: Vec<ReadResourceObject<ResourceConvoy>>,
        terminals: Vec<ReadResourceObject<ResourceTerminalSession>>,
    ) -> Result<Box<dyn MessageReceiverRead>, String>;
}

/// Projects resource and fleet state supplied by the daemon and FleetService.
/// Host refresh stays with the daemon; FleetService gathers local and replica rows.
pub struct ReadProjections<'a> {
    pub backend: &'a ResourceBackend,
    pub message_receivers: &'a dyn MessageReceiverLoader,
    pub config: &'a ConfigStore,
    pub host_registry: &'a crate::host_registry::HostRegistry,
    pub environment_manager: &'a dyn EnvironmentSummaries,
    pub host_name: &'a HostName,
    pub node_id: &'a NodeId,
    pub clock: &'a Arc<dyn Clock>,
    pub leaf_subscriptions: &'a dyn LeafSubscriptionRead,
    pub fleet: &'a FleetService,
    /// The engine's provisioning readings behind `[primitives]` (#3629 layer 4).
    pub lifecycles: &'a dyn crate::ports::LifecycleReads,
    /// The `[primitives]` Convoy path: explanations show the engine's promises (#3587 layer 8).
    pub convoy_engine: &'a flotilla_resources::convoy::engine::EngineConvoyPath,
}

impl ReadProjections<'_> {
    /// Read local and replicated convoy conditions across every stored namespace.
    pub async fn crew_stalls(
        backend: &ResourceBackend,
        full: bool,
        now: DateTime<Utc>,
    ) -> Result<flotilla_protocol::CrewStallsResponse, String> {
        let mut rows = Vec::new();
        for namespace in backend.stored_namespaces::<ResourceConvoy>().await.map_err(|error| error.to_string())? {
            let convoys = backend
                .query_declared::<ResourceConvoy>(
                    &namespace,
                    &StoreQuery {
                        field_values: BTreeMap::from([(
                            "status.phase".into(),
                            ["Pending", "Active", "Interrupted", "Landing"].map(str::to_string).into(),
                        )]),
                        any_of: Some(vec![
                            StoreQuery { field_prefixes: BTreeMap::from([("status.stalled".into(), "{".into())]), ..Default::default() },
                            StoreQuery {
                                fields: BTreeMap::from([("status.crew_work.*.*.phase".into(), "Stalled".into())]),
                                ..Default::default()
                            },
                        ]),
                        include_replicas: true,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            let projects = backend
                .definitions::<Project>(&namespace)
                .list_names(convoys.items.iter().filter_map(|source| source.object.spec.project_ref.clone()))
                .await
                .map_err(|error| error.to_string())?;
            let artifacts = backend
                .query_declared::<flotilla_resources::Artifact>(
                    &namespace,
                    &StoreQuery {
                        field_values: BTreeMap::from([(
                            "spec.convoy".into(),
                            convoys.items.iter().map(|source| source.object.metadata.name.clone()).collect(),
                        )]),
                        include_replicas: true,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            for source in convoys.items {
                let convoy = source.object;
                let Some(status) = convoy.status.as_ref().filter(|status| !status.phase.is_terminal()) else { continue };
                let mut obligations = BTreeMap::new();
                if let Some(stall) = &status.stalled {
                    for leaf in &stall.leaves {
                        if let flotilla_protocol::LeafAddress::Work { work, .. } = &leaf.address {
                            if let Some(role) = leaf.field_path.strip_prefix(".crew.").and_then(|path| path.strip_suffix(".phase")) {
                                obligations.insert((work.clone(), role.to_string()), Some(stall));
                            }
                        }
                    }
                    if obligations.is_empty() {
                        if let Some(flotilla_resources::LeafMaker::Actor { vessel, role }) = &stall.maker {
                            obligations.insert((vessel.clone(), role.clone()), Some(stall));
                        }
                    }
                }
                // A visible stall must not disappear merely because its maker or
                // leaves do not identify a crew actor (for example a controller wait).
                if obligations.is_empty() {
                    if let Some(stall) = &status.stalled {
                        obligations.insert((String::new(), String::new()), Some(stall));
                    }
                }
                // A later crew declaration can replace the convoy's visible condition.
                // Keep every stalled crew row; absent per-obligation metadata stays unknown.
                for (vessel, crew) in &status.crew_work {
                    for (role, state) in crew {
                        if state.phase == CrewWorkPhase::Stalled {
                            obligations.entry((vessel.clone(), role.clone())).or_insert(None);
                        }
                    }
                }
                let project_display_name = convoy.spec.project_ref.as_ref().and_then(|name| {
                    projects.iter().find(|project| &project.metadata.name == name).map(|project| project.spec.display_name.clone())
                });
                for ((vessel, role), stall) in obligations {
                    let evidence = stall
                        .map(|stall| stall.evidence.clone())
                        .or_else(|| status.crew_work.get(&vessel).and_then(|crew| crew.get(&role)).and_then(|state| state.message.clone()))
                        .unwrap_or_default();
                    let group = serde_json::to_string(&(
                        stall.and_then(|stall| stall.reason),
                        stall.and_then(|stall| stall.cause.as_ref()),
                        evidence.split_whitespace().collect::<Vec<_>>().join(" "),
                    ))
                    .map_err(|error| error.to_string())?;
                    let supervisor = stall
                        .and_then(|stall| stall.supervisor.as_ref())
                        .map(|supervisor| format!("{}/{}/{}", supervisor.convoy, supervisor.vessel, supervisor.role));
                    let absence = supervisor.is_none().then(|| {
                        if stall.is_some() {
                            evidence.clone()
                        } else {
                            "no current stall condition recorded for this obligation".into()
                        }
                    });
                    rows.push(
                        flotilla_protocol::CrewStallRow::builder()
                            .namespace(namespace.clone())
                            .maybe_project(convoy.spec.project_ref.clone())
                            .maybe_project_display_name(project_display_name.clone())
                            .convoy(convoy.metadata.name.clone())
                            .convoy_display_name(if convoy.spec.role.is_empty() {
                                convoy.metadata.name.clone()
                            } else {
                                format!("{} #{}", convoy.spec.role, convoy.spec.generation)
                            })
                            .vessel(vessel)
                            .role(role)
                            .maybe_rung(stall.map(|stall| stall.rung))
                            .maybe_supervisor(supervisor)
                            .maybe_supervisor_absence_reason(absence)
                            .maybe_began_at(stall.map(|stall| stall.began_at))
                            .maybe_age_seconds(stall.map(|stall| now.signed_duration_since(stall.began_at).num_seconds().max(0) as u64))
                            .maybe_proposed_disposition(stall.and_then(|stall| stall.proposed_disposition))
                            .evidence(evidence)
                            .cause_group(group)
                            .shared_cause_count(1)
                            .artifacts(
                                artifacts
                                    .items
                                    .iter()
                                    .filter(|artifact| artifact.object.spec.convoy == convoy.metadata.name)
                                    .map(|artifact| format!("artifact/{namespace}/{}", artifact.object.metadata.name))
                                    .collect(),
                            )
                            .build(),
                    );
                }
            }
        }
        let mut counts = BTreeMap::new();
        for row in &rows {
            *counts.entry(row.cause_group.clone()).or_insert(0) += 1;
        }
        for row in &mut rows {
            row.shared_cause_count = counts[&row.cause_group];
        }
        rows.sort_by(|left, right| {
            (&left.namespace, &left.project, &left.convoy, &left.vessel, &left.role).cmp(&(
                &right.namespace,
                &right.project,
                &right.convoy,
                &right.vessel,
                &right.role,
            ))
        });
        Ok(flotilla_protocol::CrewStallsResponse { observed_at: now, full, rows })
    }

    pub async fn list_hosts(&self, counts: &HashMap<EnvironmentId, HostCounts>) -> Result<HostListResponse, String> {
        Ok(self.host_registry.list_hosts(counts).await)
    }

    pub async fn dispatch_queue(
        backend: &ResourceBackend,
        namespace: &str,
        project_filter: Option<&str>,
        observed_at: DateTime<Utc>,
    ) -> Result<DispatchQueueResponse, String> {
        let projects = match project_filter {
            Some(name) => backend.definitions::<Project>(namespace).list_names([name.into()]).await.map_err(|error| error.to_string())?,
            // The unscoped queue displays every Project's dispatch entries.
            None => project_ops::read_definition_catalog::<Project>(backend, namespace).await?,
        };
        let mut entries = Vec::new();
        for project in projects {
            if project_filter.is_some_and(|filter| filter != project.metadata.name) {
                continue;
            }
            let Some(status) = project.status else { continue };
            if let Some(error) = &status.dispatch_queue_error {
                return Err(format!("dispatch readiness unavailable for {}: {error}", project.metadata.name));
            }
            let attention = status.dispatch_queue_attention.is_some();
            for entry in status.dispatch_queue {
                entries.push(
                    DispatchQueueRow::builder()
                        .namespace(project.metadata.namespace.clone())
                        .project(project.metadata.name.clone())
                        .issue(entry.issue)
                        .title(entry.title)
                        .ready_observed_at(entry.ready_observed_at)
                        .age_seconds(observed_at.signed_duration_since(entry.ready_observed_at).num_seconds().max(0) as u64)
                        .attention(attention)
                        .maybe_score(entry.score.clone())
                        .provenance(entry.provenance)
                        .build(),
                );
            }
        }
        entries.sort_by(flotilla_protocol::compare_dispatch_rows);
        Ok(DispatchQueueResponse { observed_at, entries })
    }

    pub async fn fulfilment_list(&self, namespace: &str) -> Result<FulfilmentListResponse, String> {
        let hosts = self
            .backend
            .query_declared::<ResourceHost>(
                namespace,
                &StoreQuery {
                    include_replicas: true,
                    ..StoreQuery::related_kind(
                        "metadata.name",
                        "FulfilmentKind",
                        "spec.host_ref",
                        StoreQuery { include_replicas: true, ..Default::default() },
                    )
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        let mut facts_by_host = BTreeMap::<String, ResourceHostStatus>::new();
        let mut vessel_caps = BTreeMap::<String, u32>::new();
        for host in hosts.items {
            if let (flotilla_resources::HostConnection::AgentlessSsh { .. }, Some(cap)) =
                (&host.object.spec.connection, host.object.spec.max_vessels)
            {
                vessel_caps.insert(host.object.metadata.name.clone(), cap);
            }
            if let Some(status) = host.object.status {
                facts_by_host
                    .entry(host.object.metadata.name)
                    .and_modify(|current| {
                        if current.heartbeat_at < status.heartbeat_at {
                            *current = status.clone();
                        }
                    })
                    .or_insert(status);
            }
        }
        // The fulfilment listing displays every kind, with joined host facts.
        let kinds = self
            .backend
            .query_declared::<FulfilmentKind>(namespace, &StoreQuery { include_replicas: true, ..Default::default() })
            .await
            .map_err(|error| error.to_string())?;
        let mut rows = BTreeMap::new();
        for kind in kinds.items {
            let kind = kind.object;
            if kind.metadata.deletion_timestamp.is_some() {
                continue;
            }
            let facts = facts_by_host.get(&kind.spec.host_ref).and_then(|status| status.fulfilment_facts.get(&kind.metadata.name));
            let grants = kind.spec.grants.iter().map(|grant| grant.0.clone()).collect();
            let harnesses = facts
                .map(|facts| {
                    facts
                        .harnesses
                        .iter()
                        .map(|(name, harness)| {
                            let models = harness
                                .models
                                .iter()
                                // Historical requests used a different login;
                                // never project them as current model access.
                                .filter(|(_, model)| model.source != flotilla_resources::ModelFactSource::Probe)
                                .map(|(name, model)| {
                                    (
                                        name.clone(),
                                        FulfilmentModel {
                                            usable: model.usable,
                                            source: match model.source {
                                                flotilla_resources::ModelFactSource::Probe => "probe",
                                                flotilla_resources::ModelFactSource::Declaration => "declaration",
                                            }
                                            .to_string(),
                                        },
                                    )
                                })
                                .collect();
                            (name.clone(), FulfilmentHarness { version: harness.version.clone(), models })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let row = FulfilmentRow {
                name: kind.metadata.name.clone(),
                host_ref: kind.spec.host_ref.clone(),
                pool: kind.spec.pool,
                realisation: match kind.spec.realisation {
                    FulfilmentRealisation::DockerPerVessel { .. } => "docker_per_vessel",
                    FulfilmentRealisation::HostDirect => "host_direct",
                    FulfilmentRealisation::CloudSandboxPerVessel { .. } => "cloud_sandbox_per_vessel",
                }
                .to_string(),
                grants,
                harnesses,
                toolchains: facts.map(|facts| facts.toolchains.clone()).unwrap_or_default(),
                gui_session_logged_in: facts.map(|facts| facts.gui_session_logged_in),
                free_vessel_slots: facts.and_then(|facts| facts.free_vessel_slots),
                max_vessels: vessel_caps.get(&kind.spec.host_ref).copied(),
                image: facts.and_then(|facts| facts.image.as_ref().map(|image| image.image_ref.clone())),
            };
            rows.insert((row.host_ref.clone(), row.name.clone()), row);
        }
        Ok(FulfilmentListResponse { kinds: rows.into_values().collect() })
    }

    pub async fn fleet_health(
        &self,
        namespace: &str,
        host_list: HostListResponse,
        fleet_rows: Vec<FleetListRow>,
        local_host_id: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<FleetHealthResponse, String> {
        let configured_hosts = self
            .config
            .load_hosts()
            .map(|hosts| hosts.hosts.into_iter().filter(|(_, host)| !host.agentless_ssh).collect::<HashMap<_, _>>())
            .unwrap_or_default();
        let configured_names =
            configured_hosts.values().map(|remote| HostName::new(remote.expected_host_name.clone())).collect::<HashSet<_>>();
        let configured_by_node = configured_hosts
            .values()
            .filter_map(|remote| remote.expected_node_id.clone().map(|node_id| (node_id, HostName::new(remote.expected_host_name.clone()))))
            .collect::<HashMap<_, _>>();

        let mut host_rows = BTreeMap::<HostName, (bool, bool, PeerConnectionState)>::new();
        for entry in host_list.hosts {
            let configured = entry.configured || configured_names.contains(&entry.host_name);
            host_rows
                .entry(entry.host_name)
                .and_modify(|row| {
                    row.0 |= entry.is_local;
                    row.1 |= configured;
                    if entry.connection_status == PeerConnectionState::Connected {
                        row.2 = PeerConnectionState::Connected;
                    }
                })
                .or_insert((entry.is_local, configured, entry.connection_status));
        }
        for host in configured_names {
            host_rows.entry(host).or_insert((false, true, PeerConnectionState::Disconnected));
        }
        host_rows.entry(self.host_name.clone()).or_insert((true, false, PeerConnectionState::Connected));

        let mut statuses = HashMap::<HostName, ResourceHostStatus>::new();
        let mut host_syncs = HashMap::<HostName, DateTime<Utc>>::new();
        let mut host_refs = HashMap::<String, HostName>::new();
        if let Some(host_id) = &local_host_id {
            host_refs.insert(host_id.clone(), self.host_name.clone());
        }
        let resource_hosts = self
            .backend
            .query_declared::<ResourceHost>(namespace, &StoreQuery { include_replicas: true, ..Default::default() })
            .await
            .map_err(|error| error.to_string())?;
        for source in &resource_hosts.items {
            if matches!(source.provenance, ResourceProvenance::Local)
                && local_host_id.as_deref() == Some(source.object.metadata.name.as_str())
            {
                host_refs.insert(source.object.metadata.name.clone(), self.host_name.clone());
                if let Some(status) = &source.object.status {
                    statuses.insert(self.host_name.clone(), status.clone());
                }
            }
        }
        // All fleet views select the freshest self-report when multiple origins name one host.
        for (host, report) in replicated_host_reports(&resource_hosts.items, self.host_registry, &configured_by_node).await {
            host_refs.insert(report.host_id, host.clone());
            host_syncs.insert(host.clone(), report.last_synced_at);
            if let Some(status) = report.status {
                statuses.entry(host).or_insert(status);
            }
        }

        let mut manifest_needs_by_host = HashMap::<HostName, usize>::new();
        let mut manifest_reasons_by_host = HashMap::<HostName, Vec<String>>::new();
        let manifest_roots = self
            .backend
            .query_declared::<ManifestRoot>(namespace, &StoreQuery { include_replicas: true, ..Default::default() })
            .await
            .map_err(|error| error.to_string())?;
        for root in manifest_roots.items {
            let Some(host) = host_refs.get(&root.object.spec.host) else { continue };
            if let Some(stall) = root.object.status.as_ref().and_then(|status| status.stalled.as_ref()) {
                manifest_reasons_by_host
                    .entry(host.clone())
                    .or_default()
                    .push(format!("ManifestRoot/{}: {}", root.object.metadata.name, stall.evidence));
            }
            if root
                .object
                .status
                .as_ref()
                .and_then(|status| status.stalled.as_ref())
                .is_some_and(|stalled| stalled.maker.is_none() || stalled.rung == flotilla_resources::StallRung::Operator)
            {
                *manifest_needs_by_host.entry(host.clone()).or_default() += 1;
            }
        }

        let mut fulfilments_by_host = HashMap::<HostName, Vec<FulfilmentRow>>::new();
        for kind in self.fulfilment_list(namespace).await?.kinds {
            if let Some(host_name) = host_refs.get(&kind.host_ref) {
                fulfilments_by_host.entry(host_name.clone()).or_default().push(kind);
            }
        }
        let warning_window_days = self.config.load_daemon_config().unwrap_or_default().credentials.warning_window_days;
        let credential_warning_window = chrono::Duration::days(i64::from(warning_window_days));
        let mut rows = {
            let mut counts = HashMap::<HostName, (usize, HashSet<String>)>::new();
            accumulate_fleet_health_counts(&mut counts, &fleet_rows);
            let mut surface_by_convoy = HashMap::new();
            for row in &fleet_rows {
                let Some(convoy) = &row.convoy_ref else { continue };
                surface_by_convoy
                    .entry((row.host.clone(), convoy.clone()))
                    .and_modify(|state: &mut flotilla_protocol::result_set::SurfaceState| {
                        if row.surface_state.needs_attention()
                            || (matches!(row.surface_state, flotilla_protocol::result_set::SurfaceState::StalledHandled { .. })
                                && !state.needs_attention())
                        {
                            *state = row.surface_state;
                        }
                    })
                    .or_insert(row.surface_state);
            }
            let mut rows = Vec::with_capacity(host_rows.len());
            for (host, (is_local, configured, link)) in host_rows {
                let status = statuses.get(&host);
                let last_sync = host_syncs.get(&host).copied();
                let heartbeat_at = status.and_then(|status| status.heartbeat_at);
                let heartbeat_fresh =
                    heartbeat_at.is_some_and(|at| now.signed_duration_since(at).num_seconds() <= HEARTBEAT_READY_TTL_SECS);
                let replica_fresh = is_local || last_sync.is_some_and(|at| replica_sync_is_fresh(at, now));
                let daemon_generation = status.and_then(|status| status.daemon_generation.clone());
                let replica_generation = daemon_generation.clone();
                let staleness = if heartbeat_fresh && replica_fresh {
                    FleetHostStaleness::Current
                } else if heartbeat_at.is_some() || last_sync.is_some() {
                    FleetHostStaleness::Stale
                } else {
                    FleetHostStaleness::Unknown
                };
                let observation_agreement =
                    fleet_observation_agreement(&link, heartbeat_at, heartbeat_fresh, daemon_generation.as_deref(), is_local);
                let (crew_count, convoys) = counts.remove(&host).unwrap_or_default();
                let surface_states = flotilla_protocol::FleetSurfaceCounts {
                    available: surface_by_convoy
                        .iter()
                        .filter(|((row_host, _), state)| {
                            row_host == &host && **state == flotilla_protocol::result_set::SurfaceState::Available
                        })
                        .count(),
                    stalled_handled: surface_by_convoy
                        .iter()
                        .filter(|((row_host, _), state)| {
                            row_host == &host && matches!(state, flotilla_protocol::result_set::SurfaceState::StalledHandled { .. })
                        })
                        .count(),
                    needs_you: surface_by_convoy
                        .iter()
                        .filter(|((row_host, _), state)| row_host == &host && state.needs_attention())
                        .count()
                        + manifest_needs_by_host.get(&host).copied().unwrap_or_default()
                        + usize::from(status.and_then(message_controller_failure).is_some()),
                };
                let mut degraded_conditions: Vec<String> = status
                    .into_iter()
                    .flat_map(|status| status.conditions.iter())
                    .filter(|condition| condition.value == ConditionValue::False)
                    .map(|condition| format!("{}: {}", condition.condition_type, condition.message))
                    .collect();
                degraded_conditions.extend(manifest_reasons_by_host.remove(&host).unwrap_or_default());
                let credential_attention =
                    status.map(|status| host_credential_attention(status, now, credential_warning_window)).unwrap_or_default();

                rows.push(
                    FleetHostRow::builder()
                        .fulfilments(fulfilments_by_host.remove(&host).unwrap_or_default())
                        .host(host)
                        .is_local(is_local)
                        .configured(configured)
                        .link(link)
                        .maybe_daemon_generation(daemon_generation)
                        .maybe_protocol_fingerprint(status.and_then(|status| status.protocol_fingerprint.clone()))
                        .maybe_daemon_version(status.and_then(|status| status.daemon_version.clone()))
                        .maybe_daemon_uptime_seconds(status.and_then(|status| {
                            status.daemon_started_at.map(|started_at| now.signed_duration_since(started_at).num_seconds().max(0) as u64)
                        }))
                        .maybe_heartbeat_at(heartbeat_at)
                        .maybe_replica_last_sync(if is_local { Some(now) } else { last_sync })
                        .maybe_replica_generation(replica_generation)
                        .crew_count(crew_count)
                        .convoy_count(convoys.len())
                        .surface_states(surface_states)
                        .maybe_disk_free_bytes(status.and_then(|status| status.disk_free_bytes))
                        .maybe_daemon_rss_bytes(status.and_then(|status| status.daemon_rss_bytes))
                        .maybe_blob_sync(status.and_then(|status| status.blob_sync.clone()))
                        .sleep_inhibition(status.map(|status| status.sleep_inhibition.clone()).unwrap_or_default())
                        .staleness(staleness)
                        .observation_agreement(observation_agreement)
                        .degraded_conditions(degraded_conditions)
                        .credential_attention(credential_attention)
                        .build(),
                );
            }
            rows
        };
        rows.sort_by(|left, right| right.is_local.cmp(&left.is_local).then_with(|| left.host.cmp(&right.host)));
        let dispatch_queue = Self::dispatch_queue(self.backend, namespace, None, Utc::now()).await?;
        Ok(FleetHealthResponse { hosts: rows, dispatch_queue, forge_budgets: Vec::new() })
    }

    pub async fn list_projects(backend: &ResourceBackend, namespace: &str, now: DateTime<Utc>) -> Result<ProjectListResponse, String> {
        // This listing displays every Project; reuse its complete inventory for
        // hierarchy inspection instead of reading the definitions a second time.
        let projects = project_ops::read_definition_catalog::<Project>(backend, namespace).await?;
        let fleet = backend
            .definitions::<flotilla_resources::FleetDesignation>(namespace)
            .list_names([flotilla_resources::FLEET_DESIGNATION_NAME.into()])
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .next()
            .map(|designation| designation.spec.project);
        let hierarchy = flotilla_resources::ProjectHierarchy::from_declared(
            projects.iter().map(|project| (project.metadata.name.clone(), project.spec.parent.clone())).collect(),
            fleet,
        );
        let repositories = backend
            .query_declared::<Repository>(
                namespace,
                &StoreQuery::names(
                    projects.iter().flat_map(|project| project.spec.repositories.iter().map(|repository| repository.repo.to_string())),
                    true,
                ),
            )
            .await
            .map_err(|error| error.to_string())?;
        let mut inventory = BTreeMap::new();
        // Query order preserves local-before-replica point-read precedence.
        for source in repositories.items {
            inventory.entry(source.object.metadata.name.clone()).or_insert(Ok(source.object));
        }
        let repositories = inventory
            .iter()
            .filter_map(|(name, repository)| repository.as_ref().ok().map(|repository| (RepositoryKey(name.clone()), repository.clone())))
            .collect::<Vec<_>>();
        let repository_slugs = repository_display_labels(repositories.iter().map(|(key, repository)| (key, &repository.spec)));

        let mut entries = Vec::new();
        for project in projects {
            let issue_sources = match resolve_project_issue_sources_from_inventory(&inventory, &project.spec) {
                IssueSourceResolution::Available { bindings } => bindings.into_iter().map(|binding| binding.source).collect(),
                IssueSourceResolution::Unavailable(IssueSourceUnavailable::NoIssueSource) => Vec::new(),
                IssueSourceResolution::Unavailable(error) => {
                    warn!(project = %project.metadata.name, ?error, "could not resolve project issue sources");
                    Vec::new()
                }
            };
            let conflicts = project.metadata.merge.as_ref().map(|merge| merge.conflicts.keys().cloned().collect()).unwrap_or_default();
            let mut project_repositories = BTreeMap::<RepositoryKey, BTreeSet<String>>::new();
            for repository in project.spec.repositories {
                if let Some(subpath) = repository.subpath {
                    project_repositories.entry(repository.repo).or_default().insert(subpath);
                } else {
                    project_repositories.entry(repository.repo).or_default();
                }
            }
            let repositories = project_repositories
                .into_iter()
                .map(|(key, subpaths)| ProjectListRepository {
                    slug: repository_slugs.get(&key).cloned(),
                    key,
                    subpaths: subpaths.into_iter().collect(),
                })
                .collect::<Vec<_>>();
            entries.push(
                ProjectListEntry::builder()
                    .is_fleet(hierarchy.fleet() == Some(project.metadata.name.as_str()))
                    .maybe_parent(hierarchy.parent(&project.metadata.name).map_err(|error| error.to_string())?.map(str::to_string))
                    .maybe_declaration_refused(
                        project
                            .status
                            .as_ref()
                            .and_then(|status| status.declaration_refused.as_ref())
                            .map(|refusal| refusal.message.clone()),
                    )
                    .declaration_stale(
                        project
                            .status
                            .as_ref()
                            .and_then(|status| status.declaration_refused.as_ref())
                            .is_some_and(|refusal| now - refusal.since >= DECLARATION_STALE_AFTER),
                    )
                    .namespace(project.metadata.namespace.clone())
                    .name(project.metadata.name.clone())
                    .display_name(project.spec.display_name)
                    .address(ViewAddress::Project { namespace: project.metadata.namespace, name: project.metadata.name })
                    .repositories(repositories)
                    .issue_sources(issue_sources)
                    .default_workflow_ref(project.spec.default_workflow_ref)
                    .conflicts(conflicts)
                    .build(),
            );
        }
        entries.sort_by(|left, right| (&left.namespace, &left.name).cmp(&(&right.namespace, &right.name)));
        Ok(ProjectListResponse { projects: entries })
    }

    pub async fn host_statuses(&self, namespace: &str) -> Result<BTreeMap<String, ResourceHostStatus>, String> {
        // Refresh projects every visible host status, including cached self-origin
        // authors, so this genuinely requires the complete host catalog once.
        let hosts = self
            .backend
            .query_declared::<ResourceHost>(
                namespace,
                &StoreQuery { include_replicas: true, suppress_self_origin: false, ..Default::default() },
            )
            .await
            .map_err(|error| error.to_string())?;
        let mut statuses = BTreeMap::<String, ResourceHostStatus>::new();
        for source in hosts.items {
            let host = source.object;
            if host.metadata.deletion_timestamp.is_some() {
                continue;
            }
            if let Some(status) = host.status {
                statuses
                    .entry(host.metadata.name)
                    .and_modify(|current| {
                        if current.heartbeat_at < status.heartbeat_at {
                            *current = status.clone();
                        }
                    })
                    .or_insert(status);
            }
        }
        Ok(statuses)
    }

    pub async fn get_host_status(
        &self,
        environment_id: &EnvironmentId,
        counts: &HashMap<EnvironmentId, HostCounts>,
        local_summary: &HostSummary,
    ) -> Result<HostStatusResponse, String> {
        let mut response = self.host_registry.get_host_status(environment_id, counts).await?;
        if environment_id == &local_summary.environment_id && !self.host_registry.has_resource_description(environment_id).await {
            response.visible_environments = self.environment_manager.visible_environments().await;
        }
        response.change_stream = self.fleet.change_streams().remove(&response.node.node_id);
        Ok(response)
    }

    pub async fn get_host_providers(
        &self,
        environment_id: &EnvironmentId,
        counts: &HashMap<EnvironmentId, HostCounts>,
        local_summary: &HostSummary,
    ) -> Result<HostProvidersResponse, String> {
        let mut response = self.host_registry.get_host_providers(environment_id, counts).await?;
        if environment_id == &local_summary.environment_id && !self.host_registry.has_resource_description(environment_id).await {
            response.visible_environments = self.environment_manager.visible_environments().await;
        }
        Ok(response)
    }

    pub async fn fleet_list(&self, namespace: &str, mut rows: Vec<FleetListRow>, now: DateTime<Utc>) -> Result<FleetListResponse, String> {
        let mut replicas = Vec::new();
        let host_sources = self
            .backend
            .query_declared::<ResourceHost>(namespace, &StoreQuery { include_replicas: true, ..Default::default() })
            .await
            .map_err(|error| error.to_string())?;
        let mut replicated_hosts = replicated_host_reports(&host_sources.items, self.host_registry, &HashMap::new())
            .await
            .into_iter()
            .map(|(host, report)| (host, (report.last_synced_at, report.status.and_then(|status| status.daemon_generation))))
            .collect::<HashMap<_, _>>();
        let configured_hosts = self
            .config
            .load_hosts()
            .map(|hosts| hosts.hosts.into_iter().filter(|(_, host)| !host.agentless_ssh).collect::<HashMap<_, _>>())
            .unwrap_or_default();
        let failures = self.fleet.replication_failures().await;
        let mut replication_failures_by_host = HashMap::<HostName, Vec<ResourceReplicationFailure>>::new();
        for (peer, peer_failures) in failures {
            // Source-local snapshot alarms belong to host status, not a remote
            // replica row that would incorrectly mark this home's rows unreachable.
            if &peer == self.node_id {
                continue;
            }
            let host = self.host_registry.host_name_for_node(&peer).await.unwrap_or_else(|| HostName::new(peer.as_str()));
            replication_failures_by_host
                .entry(host)
                .or_default()
                .extend(peer_failures.into_iter().map(|(kind, message)| ResourceReplicationFailure { kind, message }));
        }
        for (label, remote) in configured_hosts {
            let host = HostName::new(remote.expected_host_name);
            let replication_failures = replication_failures_by_host.remove(&host).unwrap_or_default();
            let replication_error = format_resource_replication_failures(&replication_failures);
            let source = replicated_hosts.remove(&host);
            let (reachable, last_sync, generation, message) = match source {
                Some((last_sync, generation)) => {
                    (replica_sync_is_fresh(last_sync, now) && replication_error.is_none(), Some(last_sync), generation, replication_error)
                }
                None => {
                    let unsynced = format!("replica source '{label}' has not synced yet");
                    (false, None, None, join_replica_errors(Some(&unsynced), replication_error.as_deref()))
                }
            };
            replicas.push(FleetReplicaStatus { change_stream: None, host, reachable, last_sync, generation, message });
        }
        for (host, (last_sync, generation)) in replicated_hosts {
            let replication_error =
                replication_failures_by_host.remove(&host).and_then(|failures| format_resource_replication_failures(&failures));
            replicas.push(FleetReplicaStatus {
                change_stream: None,
                host,
                reachable: replica_sync_is_fresh(last_sync, now) && replication_error.is_none(),
                last_sync: Some(last_sync),
                generation,
                message: replication_error,
            });
        }
        for (host, failures) in replication_failures_by_host {
            replicas.push(FleetReplicaStatus {
                change_stream: None,
                host,
                reachable: false,
                last_sync: None,
                generation: None,
                message: format_resource_replication_failures(&failures),
            });
        }

        for (peer, state) in self.fleet.change_streams() {
            let host = self.host_registry.host_name_for_node(&peer).await.unwrap_or_else(|| HostName::new(peer.as_str()));
            // Health alone does not establish replica inventory or reachability.
            // Unmatched healthy homes retain resident health without creating a row.
            if let Some(replica) = replicas.iter_mut().find(|replica| replica.host == host) {
                replica.change_stream = Some(state);
            }
        }

        for row in &mut rows {
            if let Some(replica) = replicas.iter().find(|replica| replica.host == row.host && !replica.reachable) {
                let message = replica.message.clone().unwrap_or_else(|| "replica sync is stale".to_string());
                row.staleness = FleetStaleness::Unreachable { last_sync: replica.last_sync, message };
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
        replicas.sort_by(|left, right| left.host.as_str().cmp(right.host.as_str()));
        let mut declaration_attention = Vec::new();
        for source in
            self.backend.query_declared::<ResourceDemand>(namespace, &attention_query(None)).await.map_err(|error| error.to_string())?.items
        {
            let demand = source.object;
            if demand.status.as_ref().is_some_and(|status| !matches!(status.state, DemandState::Raised | DemandState::Escalated)) {
                continue;
            }
            let condition = if demand.metadata.name.starts_with(DECLARATION_REFUSAL_ATTENTION_PREFIX) {
                DeclarationAttentionKind::DeclarationRefused
            } else if demand.metadata.name.starts_with(ENSURE_DRIFT_ATTENTION_PREFIX) {
                DeclarationAttentionKind::ConfigDrift
            } else {
                continue;
            };
            let mut message = demand
                .metadata
                .annotations
                .get(DECLARATION_REFUSAL_REASON_ANNOTATION)
                .or_else(|| demand.metadata.annotations.get(ENSURE_CONFIG_DRIFT_REASON_ANNOTATION))
                .cloned()
                .unwrap_or_default();
            if demand
                .metadata
                .annotations
                .get(DECLARATION_REFUSED_SINCE_ANNOTATION)
                .and_then(|since| DateTime::parse_from_rfc3339(since).ok())
                .is_some_and(|since| self.clock.now() - since.with_timezone(&Utc) >= DECLARATION_STALE_AFTER)
            {
                message.push_str(" (stale)");
            }
            declaration_attention.push(DeclarationAttentionRow { resource: demand.spec.originating_work_ref, condition, message });
        }
        declaration_attention.sort_by(|left, right| {
            (&left.resource.namespace, &left.resource.kind, &left.resource.name).cmp(&(
                &right.resource.namespace,
                &right.resource.kind,
                &right.resource.name,
            ))
        });
        let hierarchy =
            flotilla_resources::ProjectHierarchy::load_for_inspection(self.backend, namespace).await.map_err(|error| error.to_string())?;
        let fleet_project = hierarchy.fleet().map(|name| ResourceRef::new("flotilla.work/v1", "Project", namespace, name));
        Ok(FleetListResponse { fleet_project, rows, replicas, declaration_attention })
    }

    /// Select scope before any fleet bodies are read. Cross-kind joins inspect
    /// declared indexes, so discovering a crew's project does not require a
    /// second TerminalSession or Convoy read.
    pub async fn fleet_request(
        &self,
        namespace: &str,
        project: Option<&str>,
        crew_id: Option<&str>,
        convoy: Option<&str>,
    ) -> Result<FleetListResponse, String> {
        let (reads, selected_project, matching) = self.fleet_inventory(namespace, project, crew_id, convoy).await?;
        let rows = self.fleet.rows_with_backend(&reads, namespace, self.host_registry).await?;
        let mut fleet = self.with_backend(&reads).fleet_list(namespace, rows, self.clock.now()).await?;
        if let Some(selected_project) = selected_project {
            fleet.rows.retain(|row| row.convoy_ref.as_ref().is_some_and(|reference| matching.contains(reference)));
            fleet.declaration_attention.retain(|row| match row.resource.kind.as_str() {
                "Project" => row.resource.name == selected_project,
                "Convoy" => matching.contains(&row.resource.name),
                _ => false,
            });
            fleet.replicas.clear();
        }
        Ok(fleet)
    }

    pub fn with_backend<'a>(&'a self, backend: &'a ResourceBackend) -> ReadProjections<'a> {
        ReadProjections {
            backend,
            message_receivers: self.message_receivers,
            config: self.config,
            host_registry: self.host_registry,
            environment_manager: self.environment_manager,
            host_name: self.host_name,
            node_id: self.node_id,
            clock: self.clock,
            leaf_subscriptions: self.leaf_subscriptions,
            fleet: self.fleet,
            lifecycles: self.lifecycles,
            convoy_engine: self.convoy_engine,
        }
    }

    pub async fn fleet_health_request(
        &self,
        reads: &ResourceBackend,
        namespace: &str,
        host_list: HostListResponse,
        local_host_id: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<FleetHealthResponse, String> {
        let reads = reads.clone();
        let hosts = reads
            .query_declared::<ResourceHost>(namespace, &StoreQuery { include_replicas: true, ..Default::default() })
            .await
            .map_err(|error| error.to_string())?;
        // Local charter failures precede the first persisted Host heartbeat.
        let host_names = hosts
            .items
            .iter()
            .map(|source| source.object.metadata.name.clone())
            .chain(local_host_id.iter().cloned())
            .collect::<BTreeSet<_>>();
        let roots = self
            .backend
            .query_declared::<ManifestRoot>(
                namespace,
                &StoreQuery {
                    field_values: BTreeMap::from([("spec.host".into(), host_names.clone().into_iter().collect())]),
                    field_prefixes: BTreeMap::from([("status.stalled".into(), "{".into())]),
                    include_replicas: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        let kinds = self
            .backend
            .query_declared::<FulfilmentKind>(
                namespace,
                &StoreQuery {
                    field_values: BTreeMap::from([("spec.host_ref".into(), host_names.into_iter().collect())]),
                    include_replicas: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        let mut snapshot = project_ops::RequestSnapshot::new(&reads, namespace)?;
        snapshot.copy::<ResourceTerminalSession>(&reads).await?;
        snapshot.copy::<flotilla_resources::Environment>(&reads).await?;
        snapshot.copy::<ResourceHost>(&reads).await?;
        snapshot.copy::<ResourceCheckout>(&reads).await?;
        snapshot.copy::<ResourceDemand>(&reads).await?;
        snapshot.copy::<Project>(&reads).await?;
        snapshot.copy::<flotilla_resources::FleetDesignation>(&reads).await?;
        snapshot.insert::<ManifestRoot>(roots)?;
        snapshot.insert::<FulfilmentKind>(kinds)?;
        let reads = snapshot.finish();
        let rows = self.fleet.rows_with_backend(&reads, namespace, self.host_registry).await?;
        let mut response = self.with_backend(&reads).fleet_health(namespace, host_list, rows, local_host_id, now).await?;
        for host in hosts.items {
            if let Some(value) = host.object.status.and_then(|status| status.capabilities.get("forge_budgets").cloned()) {
                if let Ok(rows) = serde_json::from_value::<Vec<flotilla_protocol::ForgeBudgetRow>>(value) {
                    response.forge_budgets.extend(rows);
                }
            }
        }
        Ok(response)
    }

    pub async fn fleet_inventory(
        &self,
        namespace: &str,
        project: Option<&str>,
        crew_id: Option<&str>,
        convoy: Option<&str>,
    ) -> Result<(ResourceBackend, Option<String>, BTreeSet<String>), String> {
        use flotilla_resources::{Environment, FleetDesignation, FLEET_DESIGNATION_NAME};
        let scoped = project.is_some() || crew_id.is_some() || convoy.is_some();
        let (scope, context) = if !scoped {
            // Every convoy participates in the full fleet address catalog.
            (StoreQuery { include_replicas: true, ..Default::default() }, None)
        } else if let Some(project) = project {
            (
                StoreQuery {
                    fields: BTreeMap::from([("spec.project_ref".into(), project.into())]),
                    include_replicas: true,
                    ..Default::default()
                },
                None,
            )
        } else {
            let context = if let Some(crew_id) = crew_id {
                StoreQuery {
                    include_replicas: true,
                    ..StoreQuery::related_kind(
                        "metadata.name",
                        "TerminalSession",
                        "spec.source.context.convoy",
                        StoreQuery {
                            fields: BTreeMap::from([("status.crew.id".into(), crew_id.into())]),
                            include_replicas: true,
                            ..Default::default()
                        },
                    )
                }
            } else {
                StoreQuery::names([convoy.ok_or("scope requires a convoy")?.to_string()], true)
            };
            (
                StoreQuery { include_replicas: true, ..StoreQuery::related("spec.project_ref", "spec.project_ref", context.clone()) },
                Some(context),
            )
        };
        let convoy_query = match context.as_ref() {
            Some(context) => {
                StoreQuery { any_of: Some(vec![context.clone(), scope.clone()]), include_replicas: true, ..Default::default() }
            }
            None => scope.clone(),
        };
        let mut terminal_query = StoreQuery {
            include_replicas: true,
            ..StoreQuery::related_kind("metadata.labels.\"flotilla.work/convoy\"", "Convoy", "metadata.name", scope)
        };
        if !scoped {
            // A full fleet lists every local and replicated session.
            terminal_query = StoreQuery { include_replicas: true, ..Default::default() };
        } else if project.is_none() {
            if let Some(crew_id) = crew_id {
                terminal_query = StoreQuery {
                    any_of: Some(vec![
                        terminal_query,
                        StoreQuery { fields: BTreeMap::from([("status.crew.id".into(), crew_id.into())]), ..Default::default() },
                    ]),
                    include_replicas: true,
                    ..Default::default()
                };
            }
        }
        let sessions =
            self.backend.query_declared::<ResourceTerminalSession>(namespace, &terminal_query).await.map_err(|error| error.to_string())?;
        let convoys = self.backend.query_declared::<ResourceConvoy>(namespace, &convoy_query).await.map_err(|error| error.to_string())?;
        let context_convoy = if !scoped || project.is_some() {
            None
        } else if let Some(crew_id) = crew_id {
            let session = sessions
                .items
                .iter()
                .find(|source| source.object.status.as_ref().and_then(|status| status.crew.as_ref()).is_some_and(|crew| crew.id == crew_id))
                .ok_or_else(|| format!("unknown FLOTILLA_CREW_ID `{crew_id}`"))?;
            match &session.object.spec.source {
                TerminalSessionSource::Agent { context, .. } => Some(context.convoy.clone()),
                TerminalSessionSource::Tool { .. } => return Err(format!("crew identity `{crew_id}` belongs to a non-agent process")),
            }
        } else {
            convoy.map(str::to_string)
        };
        let selected_project = match project {
            _ if !scoped => None,
            Some(project) => Some(project.to_string()),
            None => {
                let name = context_convoy.expect("scope has context");
                Some(
                    convoys
                        .items
                        .iter()
                        .find(|source| source.object.metadata.name == name)
                        .ok_or_else(|| format!("crew convoy `{name}` not found"))?
                        .object
                        .spec
                        .project_ref
                        .clone()
                        .ok_or_else(|| format!("crew convoy `{name}` has no project"))?,
                )
            }
        };
        let matching = convoys
            .items
            .iter()
            .filter(|source| selected_project.as_deref().is_none_or(|project| source.object.spec.project_ref.as_deref() == Some(project)))
            .map(|source| source.object.metadata.name.clone())
            .collect::<BTreeSet<_>>();
        let environments = self
            .backend
            .query_declared::<Environment>(
                namespace,
                &StoreQuery::names(sessions.items.iter().map(|source| source.object.spec.env_ref.clone()), false),
            )
            .await
            .map_err(|error| error.to_string())?;
        let mut host_names = environments
            .items
            .iter()
            .filter_map(|source| {
                source
                    .object
                    .spec
                    .host_direct
                    .as_ref()
                    .map(|spec| spec.host_ref.clone())
                    .or_else(|| source.object.spec.docker.as_ref().map(|spec| spec.host_ref.clone()))
            })
            .collect::<BTreeSet<_>>();
        let summary = self.host_registry.local_host_summary().await;
        if let Some(host) = summary.environment_id.host_id() {
            host_names.insert(host.to_string());
        }
        for origin in sessions
            .items
            .iter()
            .filter_map(|source| match &source.provenance {
                ResourceProvenance::Replica { origin_root, .. } => Some(origin_root),
                _ => None,
            })
            .collect::<BTreeSet<_>>()
        {
            if let Some(environment) = self.host_registry.environment_id_for_node(origin).await {
                if let Some(host) = environment.host_id() {
                    host_names.insert(host.to_string());
                }
            }
        }
        let hosts = self
            .backend
            .query_declared::<ResourceHost>(
                namespace,
                &if scoped {
                    StoreQuery { suppress_self_origin: false, ..StoreQuery::names(host_names, true) }
                } else {
                    // Every visible host contributes to full fleet replica reports.
                    StoreQuery { include_replicas: true, suppress_self_origin: false, ..Default::default() }
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        let checkouts = self
            .backend
            .query_declared::<ResourceCheckout>(
                namespace,
                &StoreQuery {
                    field_values: BTreeMap::from([(
                        "metadata.labels.\"flotilla.work/convoy\"".into(),
                        matching.clone().into_iter().collect(),
                    )]),
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        let demands = self
            .backend
            .query_declared::<ResourceDemand>(namespace, &attention_query(selected_project.as_deref().map(|project| (project, &matching))))
            .await
            .map_err(|error| error.to_string())?;
        // The graph is also the fleet-designation projection's complete catalog.
        let projects = project_ops::read_definition_catalog::<Project>(self.backend, namespace).await?;
        let fleet = self
            .backend
            .definitions::<FleetDesignation>(namespace)
            .list_names([FLEET_DESIGNATION_NAME.into()])
            .await
            .map_err(|error| error.to_string())?;
        let mut snapshot = project_ops::RequestSnapshot::new(self.backend, namespace)?;
        snapshot.insert::<ResourceTerminalSession>(sessions)?;
        snapshot.insert::<ResourceConvoy>(convoys)?;
        snapshot.insert::<Environment>(environments)?;
        snapshot.insert::<ResourceHost>(hosts)?;
        snapshot.insert::<ResourceCheckout>(checkouts)?;
        snapshot.insert::<ResourceDemand>(demands)?;
        snapshot.insert_definitions::<Project>(projects)?;
        snapshot.insert_definitions::<FleetDesignation>(fleet)?;
        Ok((snapshot.finish(), selected_project, matching))
    }

    pub async fn explain_convoy(&self, namespace: &str, name: &str) -> Result<ConvoyExplanation, String> {
        self.explain_convoy_with_resource(namespace, name).await.map(|(explanation, _)| explanation)
    }

    /// Keep the resolved, engine-viewed resource available for explanation enrichment.
    pub async fn explain_convoy_with_resource(
        &self,
        namespace: &str,
        name: &str,
    ) -> Result<(ConvoyExplanation, ResourceObject<ResourceConvoy>), String> {
        let mut candidates = vec![StoreQuery::names([name.to_string()], true)];
        if let Ok((role, project)) = flotilla_resources::parse_role_address(name) {
            let mut labels = BTreeMap::from([(ROLE_LABEL.into(), role.into())]);
            if let Some(project) = project {
                labels.insert(PROJECT_LABEL.into(), project.into());
            }
            candidates.push(StoreQuery::labels(&labels, true, true));
        }
        let selected_query =
            StoreQuery { any_of: (!candidates.is_empty()).then_some(candidates), include_replicas: true, ..Default::default() };
        let mut linked = Vec::new();
        for target in ["spec.subjects.*.subject", "status.subjects.*.subject"] {
            for source in ["spec.subjects.*.subject", "status.subjects.*.subject"] {
                linked.push(StoreQuery::related(target, source, selected_query.clone()));
            }
        }
        // Select candidate identities and linked landed evidence in one indexed
        // operation; the address resolver still owns exact-name and ambiguity rules.
        let convoy_sources = self
            .backend
            .query_declared::<ResourceConvoy>(
                namespace,
                &StoreQuery {
                    any_of: Some(vec![
                        selected_query,
                        StoreQuery {
                            fields: BTreeMap::from([("status.phase".into(), "Landed".into())]),
                            any_of: (!linked.is_empty()).then_some(linked),
                            ..Default::default()
                        },
                    ]),
                    include_replicas: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        let identities = convoy_sources
            .items
            .iter()
            .map(|source| ConvoyAddressIdentity {
                record_name: &source.object.metadata.name,
                role: source.object.metadata.labels.get(ROLE_LABEL).map(String::as_str),
                project: source.object.metadata.labels.get(PROJECT_LABEL).map(String::as_str),
                terminal: source.object.status.as_ref().is_some_and(|status| status.phase.is_terminal()),
            })
            .collect::<Vec<_>>();
        let selected = resolve_convoy_candidate_indices(&identities, name)?;
        let convoy_source = selected
            .into_iter()
            .map(|index| &convoy_sources.items[index])
            .max_by_key(|source| matches!(source.provenance, ResourceProvenance::Local))
            .ok_or_else(|| format!("no convoy matches `{name}`"))?;
        // With the engine holding the convoy, the promises explained are the engine's (#3587 layer 8).
        let convoy = flotilla_resources::convoy::engine::viewed_read(self.convoy_engine, convoy_source)?;
        let now = self.clock.now();
        let (receivers, project, receiver_aliases) =
            convoy_receiver_evidence(self.message_receivers, self.backend, namespace, &convoy_sources.items, &convoy).await?;
        let repositories = self
            .backend
            .query_declared::<Repository>(
                namespace,
                &StoreQuery::names(
                    convoy.spec.repositories.iter().map(|repository| repository.repo_ref.to_string()).chain(
                        project.iter().flat_map(|project| project.spec.repositories.iter().map(|repository| repository.repo.to_string())),
                    ),
                    true,
                ),
            )
            .await
            .map_err(|error| error.to_string())?;
        let forge_names = repositories
            .items
            .iter()
            .filter_map(|source| match source.object.spec.identity() {
                flotilla_resources::RepositoryIdentity::Forge { forge_ref, .. } => Some(forge_ref.clone()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let forges = self
            .backend
            .definitions::<Forge>(namespace)
            .list_names(forge_names)
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|forge| forge.spec)
            .collect::<Vec<_>>();
        let repository_specs =
            repositories.items.iter().map(|record| (record.object.spec.key(), &record.object.spec)).collect::<HashMap<_, _>>();
        let reference_context = flotilla_resources::convoy_reference_context(
            &convoy.spec.repositories,
            convoy.spec.project_ref.as_deref(),
            project.as_ref().map(|project| &project.spec),
            &forges,
            |key| repository_specs.get(key).copied(),
        );
        let subjects = convoy_subject_rows(&convoy, &reference_context);
        let change_request_stale_after = self.leaf_subscriptions.change_request_stale_after();

        let checkout_sources = self
            .backend
            .query_declared::<ResourceCheckout>(
                namespace,
                &StoreQuery::labels(&BTreeMap::from([(CONVOY_LABEL.into(), convoy.metadata.name.clone())]), true, true),
            )
            .await
            .map_err(|error| error.to_string())?
            .items;
        let selected_checkout_sources = flotilla_resources::select_convoy_child_sources(&convoy, &checkout_sources);
        let selected_checkouts = selected_checkout_sources.iter().map(|(name, source)| (name.clone(), source.object.clone())).collect();
        let vessel_sources = self
            .backend
            .query_declared::<Vessel>(
                namespace,
                &StoreQuery::labels(&BTreeMap::from([(CONVOY_LABEL.into(), convoy.metadata.name.clone())]), true, true),
            )
            .await
            .map_err(|error| error.to_string())?
            .items;
        let selected_vessels = flotilla_resources::select_convoy_children(&convoy, &vessel_sources);
        let expected = expected_checkout_refs(&convoy).map_err(|error| format!("derive expected checkouts: {error}"))?;
        let checkouts = expected
            .iter()
            .map(|checkout_name| {
                let source = selected_checkout_sources.get(checkout_name);
                let selected = source.map(|source| &source.object);
                let provenance = source.map(|source| explained_provenance(&source.provenance, self.node_id));
                // One keyed engine read per checkout, at the checkout's home.
                let lifecycle = source
                    .map(|source| {
                        let origin = match &source.provenance {
                            flotilla_resources::ResourceProvenance::Local => None,
                            flotilla_resources::ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.to_string()),
                        };
                        self.lifecycles.try_reading(crate::ports::ProvisioningKind::Checkout, checkout_name, origin.as_deref())
                    })
                    .transpose()?
                    .flatten();
                let integration = selected.and_then(|checkout| checkout.status.as_ref()).map(|status| &status.integration);
                Ok(ExplainedCheckout {
                    name: checkout_name.clone(),
                    observed: selected.is_some(),
                    provenance,
                    clean: integration.map(|status| explain_condition(&status.clean, now, LANDING_EVIDENCE_TTL)),
                    pushed: integration.map(|status| explain_condition(&status.pushed, now, LANDING_EVIDENCE_TTL)),
                    landed: integration.map(|status| explain_condition(&status.landed, now, LANDING_EVIDENCE_TTL)),
                    lifecycle,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        let change_request_sources = self
            .backend
            .query_declared::<flotilla_resources::ChangeRequest>(
                namespace,
                &StoreQuery {
                    any_of: Some(
                        subjects
                            .iter()
                            .filter(|row| row.subject.kind == SubjectKind::ChangeRequest)
                            .map(|row| StoreQuery {
                                fields: BTreeMap::from([
                                    ("spec.service".into(), row.subject.source.service.clone()),
                                    ("spec.scope".into(), row.subject.source.scope.clone()),
                                    ("spec.number".into(), row.subject.id.clone()),
                                ]),
                                ..Default::default()
                            })
                            .chain(std::iter::once(StoreQuery::names(
                                expected_change_request_leaves(&convoy, &forges)
                                    .map_err(|error| error.to_string())?
                                    .iter()
                                    .filter_map(|leaf| leaf_record_name(&leaf.address, &forges).ok()),
                                true,
                            )))
                            .collect(),
                    ),
                    include_replicas: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| error.to_string())?
            .items;
        let selected_change_requests =
            flotilla_resources::select_change_request_sources(change_request_sources.iter().map(|source| (&source.object, source)));
        let change_request_objects = selected_change_requests
            .values()
            .filter_map(|(object, _)| Some((spec_record_name(&object.spec, &forges)?, object.clone())))
            .collect::<BTreeMap<_, _>>();
        // Match the shared catalog: any linked landed convoy makes a merged
        // request closed, even when the convoy being explained is still live.
        let landed_subjects = convoy_sources
            .items
            .iter()
            .filter(|source| source.object.status.as_ref().is_some_and(|status| status.phase == flotilla_resources::ConvoyPhase::Landed))
            .flat_map(|source| convoy_subject_rows(&source.object, &reference_context))
            .map(|row| row.subject)
            .collect::<BTreeSet<_>>();
        let observations_by_subject = selected_change_requests
            .iter()
            .map(|(subject, (object, _))| (subject.clone(), object.status.as_ref()))
            .collect::<BTreeMap<_, _>>();
        let subject_observations = subjects
            .iter()
            .filter(|row| row.subject.kind == SubjectKind::ChangeRequest)
            .map(|row| {
                let status = observations_by_subject.get(&row.subject).copied().flatten();
                explain_subject_observation(&row.subject, status, landed_subjects.contains(&row.subject), now, change_request_stale_after)
            })
            .collect();
        let expected_change_request_leaves =
            expected_change_request_leaves(&convoy, &forges).map_err(|error| format!("derive expected change requests: {error}"))?;
        let expected_change_requests = expected_change_request_leaves
            .iter()
            .filter(|leaf| matches!(leaf.address, flotilla_protocol::LeafAddress::ChangeRequest { .. }))
            .filter_map(|leaf| leaf_record_name(&leaf.address, &forges).ok())
            .collect::<BTreeSet<_>>();
        let bound_name = bound_change_request_address(&convoy, &forges)
            .map_err(|error| format!("derive bound change request: {error}"))?
            .and_then(|address| record_name(&address));
        let mut observation_errors = BTreeMap::new();
        for leaf in expected_change_request_leaves {
            let Some(subject) =
                leaf_address(&leaf.address, &forges).ok().and_then(|address| ChangeRequestKey::new(namespace, address).ok())
            else {
                continue;
            };
            if let Some(error) = self.leaf_subscriptions.change_request_observation_error(&subject).await {
                observation_errors.insert(subject.record_name(), error.to_string());
            }
        }
        let change_requests = expected_change_requests
            .iter()
            .map(|record_name| {
                let selected = change_request_objects.get(record_name);
                let provenance = selected_change_requests
                    .values()
                    .find(|(object, _)| spec_record_name(&object.spec, &forges).as_ref() == Some(record_name))
                    .map(|(_, source)| explained_provenance(&source.provenance, self.node_id));
                let observed_at = selected.and_then(|source| source.status.as_ref()).map(|status| status.state.observed_at);
                ExplainedChangeRequest {
                    name: record_name.clone(),
                    bound: bound_name.as_ref() == Some(record_name),
                    observed: selected.is_some(),
                    provenance,
                    fields: selected.and_then(|source| serde_json::to_value(source).ok()),
                    observed_at: observed_at.map(|at| at.to_rfc3339()),
                    freshness: observed_freshness(observed_at, now, change_request_stale_after),
                    observation_error: observation_errors.get(record_name).cloned(),
                }
            })
            .collect::<Vec<_>>();

        let evaluation = evaluate_landing_settlement(
            &convoy,
            &selected_vessels,
            &selected_checkouts,
            &change_request_objects,
            &forges,
            change_request_stale_after,
            LANDING_EVIDENCE_TTL,
            now,
        );
        let mut unmet = evaluation.unmet.into_iter().map(explain_unmet_expectation).collect::<Vec<_>>();
        unmet.extend(observation_errors.iter().map(|(record, error)| ExplainedUnmetExpectation {
            reason: "observation_error".to_string(),
            subject: format!("change_request/{record}"),
            detail: error.clone(),
        }));
        let settlement = ExplainedSettlement {
            mode: match evaluation.mode {
                SettlementMode::NoExit => flotilla_protocol::commands::SETTLEMENT_MODE_STANDING,
                SettlementMode::ClaimExit => "claim_exit",
                SettlementMode::WorldTerminal => "world_terminal",
                SettlementMode::ObservedDigest => "observed_digest",
            }
            .to_string(),
            satisfied: evaluation.satisfied,
            unmet,
        };

        let subscriptions = self
            .leaf_subscriptions
            .diagnostics()
            .await
            .into_iter()
            .filter(|(row, _)| matches!(&row.watcher, LeafWatcher::ReconcilerWake { convoy } | LeafWatcher::TurnDelivery { convoy, .. } if convoy == name))
            .map(|(row, firings)| ExplainedSubscription {
                id: row.id,
                watcher: match row.watcher {
                    LeafWatcher::WaitCaller { .. } => "wait_caller",
                    LeafWatcher::ReconcilerWake { .. } => "reconciler_wake",
                    LeafWatcher::TurnDelivery { .. } => "turn_delivery",
                }
                .to_string(),
                leaves: row.leaves,
                last_leaf_firings: firings
                    .into_iter()
                    .map(|firing| ExplainedLeafFiring { leaf: firing.leaf, value: firing.value, fired_at: firing.fired_at.to_rfc3339() })
                    .collect(),
            })
            .collect();

        let observed_sessions = receivers.terminals();
        let unclaimed_work = explained_unclaimed_work(convoy.status.as_ref(), observed_sessions, name);
        let selected_sessions = flotilla_resources::select_convoy_children(&convoy, observed_sessions);
        let queued_turns = convoy
            .status
            .as_ref()
            .into_iter()
            .flat_map(|status| &status.turn_deliveries)
            .flat_map(|(source, delivery)| {
                delivery.episodes.iter().filter_map(|episode| {
                    let flotilla_resources::TurnDeliveryOutcome::Queued { rung, queued_at, vessel, role, message_id, .. } =
                        &episode.outcome
                    else {
                        return None;
                    };
                    let session = flotilla_resources::queued_turn_session(&selected_sessions, vessel, role);
                    let (confirmed, blocking_reason) = flotilla_resources::queued_turn_evidence(session, message_id);
                    // A receipt can arrive between health ticks; explain must not call it blocked.
                    if confirmed {
                        return None;
                    }
                    let age = now.signed_duration_since(*queued_at);
                    Some(flotilla_protocol::commands::ExplainedQueuedTurn {
                        source: source.clone(),
                        subject_revision: episode.subject_revision.clone(),
                        vessel: vessel.clone(),
                        role: role.clone(),
                        message_id: message_id.clone(),
                        rung: *rung,
                        queued_at: queued_at.to_rfc3339(),
                        age_seconds: age.num_seconds().max(0),
                        blocking_reason,
                        overdue: age > flotilla_resources::TurnDeliveryOutcome::QUEUED_BOUND,
                    })
                })
            })
            .collect();
        let mut crew_deliveries = observed_sessions
            .iter()
            .filter(|source| source.object.metadata.labels.get(CONVOY_LABEL).is_some_and(|convoy| convoy == name))
            .cloned()
            .map(|source| ExplainedCrewDelivery {
                terminal_condition: source.object.status.as_ref().and_then(crate::terminal_health::condition),
                obstruction: source.object.status.as_ref().and_then(|status| {
                    let session = status.session_id.as_deref().unwrap_or(&source.object.metadata.name);
                    status
                        .attention_obstruction()
                        .filter(|_| status.phase == flotilla_resources::TerminalSessionPhase::Running)
                        .map(|obstruction| obstruction.describe(session))
                }),
                session: source.object.metadata.name,
                role: source.object.spec.role,
                // ADR 0028's delivery ladder has not landed yet. Keep the
                // field explicit so recorded rungs appear without inventing
                // one from session liveness or message delivery.
                last_delivery_rung: None,
                sender: match &source.object.spec.source {
                    TerminalSessionSource::Agent { message: Some(message), .. } if message.sender != CrewMessageSender::Unknown => {
                        Some(flotilla_resources::legacy_message_sender(&message.sender).0)
                    }
                    _ => None,
                },
                pending_briefs: match &source.object.spec.source {
                    TerminalSessionSource::Agent { message: Some(message), .. } => message
                        .pending_after(source.object.status.as_ref().and_then(|status| status.delivered_message_id.as_deref()))
                        .into_iter()
                        .filter(|message| {
                            matches!(message.sender, CrewMessageSender::OperatorResume { .. } | CrewMessageSender::OperatorFollowUp { .. })
                        })
                        .map(|message| message.text.clone())
                        .collect(),
                    _ => Vec::new(),
                },
                delivered_message_id: source.object.status.and_then(|status| status.delivered_message_id),
            })
            .collect::<Vec<_>>();
        crew_deliveries.sort_by(|left, right| left.session.cmp(&right.session));

        let recent_events = match EventRecorder::new(self.backend.clone())
            .recent_matching_label(namespace, CONVOY_LABEL, &convoy.metadata.name, Utc::now())
            .await
        {
            Ok(events) => events
                .into_iter()
                .map(|event| ExplainedEvent {
                    reason: event.spec.reason,
                    message: event.spec.message,
                    count: event.spec.count,
                    first_seen: event.spec.first_seen.to_rfc3339(),
                    last_seen: event.spec.last_seen.to_rfc3339(),
                })
                .collect(),
            Err(error) => {
                warn!(convoy = %name, %error, "failed to enrich convoy explanation with recent events");
                Vec::new()
            }
        };

        let lifecycle_mutations = convoy
            .status
            .as_ref()
            .into_iter()
            .flat_map(|status| &status.lifecycle_mutations)
            .map(|mutation| flotilla_protocol::ExplainedLifecycleMutation {
                action: mutation.action.clone(),
                caller: mutation.caller.clone(),
                at: mutation.at.to_rfc3339(),
            })
            .collect();
        let pinned_workflow = self
            .backend
            .definitions::<WorkflowTemplate>(namespace)
            .list_names([flotilla_resources::pinned_workflow_ref(&convoy).to_string()])
            .await
            .ok()
            .and_then(|objects| objects.into_iter().next())
            .map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local });
        let role_vessels = convoy
            .status
            .as_ref()
            .and_then(|status| status.workflow_snapshot.as_ref())
            .map(|snapshot| snapshot.vessels.clone())
            .or_else(|| pinned_workflow.as_ref().map(|workflow| workflow.object.spec.vessels.clone()))
            .unwrap_or_default();

        let stores = self.config.load_daemon_config()?.blob_stores;
        let artifact_sources = self
            .backend
            .query_declared::<flotilla_resources::Artifact>(
                namespace,
                &StoreQuery {
                    fields: BTreeMap::from([("spec.convoy".into(), convoy.metadata.name.clone())]),
                    include_replicas: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .filter(|item| item.object.spec.convoy == convoy.metadata.name)
            .collect::<Vec<_>>();
        let decision_ledgers = explained_decision_ledgers(&convoy.metadata.name, convoy.status.as_ref(), &artifact_sources);
        let mut artifacts = artifact_sources
            .into_iter()
            .map(|item| ExplainedArtifact {
                kind: item.object.spec.kind.clone(),
                address: format!("artifact/{}", item.object.metadata.name),
                view_url: flotilla_config::artifact_view_url(&item.object.spec, &stores),
            })
            .collect::<Vec<_>>();
        artifacts.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.address.cmp(&b.address)));

        let (messages, held) = message_evidence(self.backend, namespace, receivers.as_ref(), Some(&convoy), &receiver_aliases).await?;
        let mut warnings: Vec<_> = convoy.spec.issues.iter().flat_map(|issue| issue.placement_needs().1).collect();
        let mut host_names = selected_vessels
            .values()
            .filter_map(|vessel| {
                vessel.status.as_ref()?.placement_decision.as_ref().map(|decision| decision.target_host.reference.to_string())
            })
            .collect::<BTreeSet<_>>();
        if let Some(decision) = convoy.status.as_ref().and_then(|status| status.placement_decision.as_ref()) {
            host_names.insert(decision.target_host.reference.to_string());
        }
        let home_environment = match &convoy_source.provenance {
            ResourceProvenance::Local => Some(self.host_registry.local_host_summary().await.environment_id),
            ResourceProvenance::Replica { origin_root, .. } => self.host_registry.environment_id_for_node(origin_root).await,
        };
        if let Some(host) = home_environment.as_ref().and_then(|environment| environment.host_id()) {
            host_names.insert(host.to_string());
        }
        // Sessions can predate placement evidence. Include their realized
        // environment hosts, without attributing an unrelated host's failure.
        let environments = self
            .backend
            .query_declared::<flotilla_resources::Environment>(
                namespace,
                &StoreQuery::names(selected_sessions.values().map(|session| session.spec.env_ref.clone()), true),
            )
            .await
            .map_err(|error| error.to_string())?;
        for environment in environments.items {
            if let Some(host) = crate::fleet::resource_environment_host_ref(&environment.object) {
                host_names.insert(host.to_string());
            }
        }
        warnings.extend(message_controller_warnings(self.backend, namespace, host_names).await?);
        warnings.sort();
        warnings.dedup();
        let explanation = ConvoyExplanation {
            warnings,
            dispatch_scores: BTreeMap::new(),
            dispatch_score_error: None,
            degraded_checkouts: selected_vessels
                .values()
                .flat_map(|vessel| {
                    vessel.status.iter().flat_map(|status| {
                        status.degraded.iter().map(|(checkout, message)| (format!("{}/{checkout}", vessel.metadata.name), message.clone()))
                    })
                })
                .collect(),
            naming: convoy.status.as_ref().and_then(|status| status.naming.clone()),
            landing_entry: convoy
                .status
                .as_ref()
                .and_then(|status| status.landing_entry.as_ref())
                .map(|entry| serde_json::to_value(entry).expect("serialize landing entry")),
            promises: convoy
                .status
                .as_ref()
                .into_iter()
                .flat_map(|status| status.promises.values())
                .flat_map(BTreeMap::values)
                .flatten()
                .map(|promise| serde_json::to_value(promise).expect("serialize promise"))
                .collect(),
            landing_settlement: convoy
                .status
                .as_ref()
                .and_then(|status| status.landing_settlement.as_ref())
                .map(|evidence| serde_json::to_value(evidence).expect("serialize landing settlement")),
            holds: convoy
                .status
                .as_ref()
                .into_iter()
                .flat_map(|status| status.turn_delivery_holds())
                .map(|hold| flotilla_protocol::commands::ExplainedTurnDeliveryHold {
                    source: hold.source,
                    reason: hold.reason,
                    raised_at: hold.raised_at.to_rfc3339(),
                })
                .collect(),
            cascade: convoy
                .status
                .as_ref()
                .and_then(|status| status.workflow_snapshot.as_ref())
                .and_then(|workflow| workflow.cascade.as_deref())
                .or_else(|| pinned_workflow.as_ref().and_then(|workflow| workflow.object.spec.cascade.as_deref()))
                .map(|cascade| serde_json::to_value(cascade).expect("serialize cascade")),

            messages,
            held_deliveries: held
                .into_iter()
                .map(|batch| flotilla_protocol::commands::ExplainedHeldDelivery {
                    description: batch.describe(namespace),
                    clear_command: batch.clear_command(namespace),
                    batch: batch.batch,
                    receiver: batch.receiver,
                    members: batch.members,
                    reason: batch.reason,
                })
                .collect(),
            environment_observations: convoy.status.as_ref().map(|status| status.environment_observations.clone()).unwrap_or_default(),
            skills: convoy
                .status
                .as_ref()
                .and_then(|status| status.workflow_snapshot.as_ref())
                .map(|workflow| {
                    workflow
                        .vessels
                        .iter()
                        .flat_map(|vessel| {
                            vessel.crew.iter().map(move |crew| {
                                (
                                    format!("{}/{}", vessel.name, crew.role),
                                    serde_json::to_value(&crew.skills).expect("serialize skill explanation"),
                                )
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            namespace: namespace.to_string(),
            convoy: name.to_string(),
            phase: convoy.status.as_ref().map_or_else(|| "Unknown".to_string(), |status| format!("{:?}", status.phase)),
            role_needs: role_vessels
                .iter()
                .flat_map(|vessel| vessel.crew.iter().map(move |crew| (format!("{}/{}", vessel.name, crew.role), crew)))
                .map(|(role, crew)| (role, crew.needs.iter().map(ToString::to_string).collect()))
                .collect(),
            allocation: pinned_workflow
                .as_ref()
                .into_iter()
                .flat_map(|workflow| &workflow.object.spec.allocation)
                .map(|decision| flotilla_protocol::commands::ExplainedAllocation {
                    vessel: decision.vessel.clone(),
                    roles: decision.roles.clone(),
                    reason: decision.reason.clone(),
                    crossed_handoffs: decision.crossed_handoffs.clone(),
                })
                .collect(),
            placement: convoy.status.as_ref().and_then(|status| status.placement_decision.clone()),
            vessel_placements: convoy
                .metadata
                .annotations
                .get(flotilla_resources::VESSEL_PLACEMENTS_ANNOTATION)
                .and_then(|encoded| serde_json::from_str::<BTreeMap<String, flotilla_resources::VesselPlacementPin>>(encoded).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|(vessel, pin)| (vessel, pin.decision))
                .collect(),
            stalled: convoy
                .status
                .as_ref()
                .and_then(|status| status.stalled.as_ref())
                .and_then(|condition| serde_json::to_value(condition).ok()),
            message: convoy.status.as_ref().and_then(|status| status.message.clone()),
            evidence_ttl_seconds: LANDING_EVIDENCE_TTL.as_secs(),
            change_request_stale_after_seconds: change_request_stale_after.as_secs(),
            checkouts,
            subjects,
            subject_observations,
            change_requests,
            subscriptions,
            crew_deliveries,
            queued_turns,
            unclaimed_work,
            decision_ledgers,
            artifacts,
            settlement,
            recent_events,
            lifecycle_mutations,
        };
        Ok((explanation, convoy))
    }
}

pub fn explained_unclaimed_work(
    status: Option<&ConvoyStatus>,
    sessions: &[ReadResourceObject<ResourceTerminalSession>],
    convoy_name: &str,
) -> Vec<ExplainedUnclaimedWork> {
    let Some(status) = status.filter(|status| !status.phase.is_terminal()) else { return Vec::new() };
    status
        .crew_work
        .iter()
        .flat_map(|(vessel, crew)| {
            crew.iter().filter_map(move |(role, state)| {
                if state.phase != CrewWorkPhase::Working {
                    return None;
                }
                let session = sessions.iter().find(|source| {
                    let labels = &source.object.metadata.labels;
                    labels.get(CONVOY_LABEL).is_some_and(|value| value == convoy_name)
                        && labels.get(VESSEL_LABEL).is_some_and(|value| value == vessel)
                        && labels.get(ROLE_LABEL).is_some_and(|value| value == role)
                });
                let evidence = if status.work.get(vessel).is_some_and(|work| work.phase == ResourceWorkPhase::Complete) {
                    "work_complete"
                } else if session.is_some_and(|source| {
                    source.object.status.as_ref().is_some_and(|status| status.phase == ResourceTerminalSessionPhase::Stopped)
                }) {
                    "session_stopped"
                } else if session.is_some_and(|source| {
                    source.object.status.as_ref().is_some_and(|status| {
                        status.attention.as_ref().is_some_and(|attention| attention.state == TerminalAttentionState::Idle)
                    })
                }) {
                    "turn_idle"
                } else {
                    return None;
                };
                Some(ExplainedUnclaimedWork { vessel: vessel.clone(), role: role.clone(), evidence: evidence.to_string() })
            })
        })
        .collect()
}

pub fn explained_decision_ledgers(
    convoy_name: &str,
    status: Option<&ConvoyStatus>,
    artifacts: &[ReadResourceObject<flotilla_resources::Artifact>],
) -> Vec<ExplainedDecisionLedger> {
    status
        .into_iter()
        .flat_map(|status| &status.crew_work)
        .flat_map(|(vessel, crew)| {
            crew.iter().flat_map(move |(role, claim)| {
                let mut ledgers = claim
                    .superseded_claims
                    .iter()
                    .map(|superseded| ExplainedDecisionLedger {
                        vessel: vessel.clone(),
                        role: role.clone(),
                        claimed_at: Some(superseded.claimed_at.to_rfc3339()),
                        // The mutable artifact key describes the current turn, not this historical claim.
                        artifact_address: None,
                        projection_missing: false,
                        projection_error: None,
                        comment_url: superseded.decision_ledger_ref.clone(),
                        missing: superseded.decision_ledger_ref.is_none(),
                        override_principal: superseded.completion_override.as_ref().map(|override_| override_.principal.clone()),
                        completed_while_crew_active: superseded.completed_while_crew_active,
                        message: superseded.message.clone(),
                        superseded: true,
                    })
                    .collect::<Vec<_>>();
                if matches!(claim.phase, CrewWorkPhase::Done | CrewWorkPhase::HandedBack) {
                    let name =
                        flotilla_resources::address::artifact_slot_record_name(convoy_name, role, "decision-ledger", convoy_name).ok();
                    let artifact = artifacts
                        .iter()
                        .filter(|source| name.as_ref() == Some(&source.object.metadata.name))
                        .min_by_key(|source| !matches!(source.provenance, ResourceProvenance::Local))
                        .filter(|source| {
                            let artifact = &source.object.spec;
                            match claim.decision_ledger_digest.as_deref() {
                                Some(digest) => artifact.digest == digest,
                                // Legacy claims have no admitted digest. When a hand-back has dated
                                // evidence, exclude bodies recorded after its claim; undated records
                                // remain readable under the previous-generation compatibility contract.
                                None => {
                                    claim.phase != CrewWorkPhase::HandedBack
                                        || artifact.recorded_at.zip(claim.finished_at).is_none_or(|(recorded, claimed)| recorded <= claimed)
                                }
                            }
                        });
                    let comment_url = artifact
                        .and_then(|source| source.object.spec.summary.get("comment_url"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                        .or_else(|| claim.decision_ledger_ref.clone());
                    let projection_error = artifact
                        .and_then(|source| source.object.spec.summary.get("projection_error"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);
                    ledgers.push(ExplainedDecisionLedger {
                        vessel: vessel.clone(),
                        role: role.clone(),
                        claimed_at: claim.finished_at.map(|at| at.to_rfc3339()),
                        artifact_address: artifact.map(|source| format!("artifact/{}", source.object.metadata.name)),
                        projection_missing: false,
                        projection_error,
                        comment_url,
                        missing: artifact.is_none(),
                        override_principal: claim.completion_override.as_ref().map(|override_| override_.principal.clone()),
                        completed_while_crew_active: claim.completed_while_crew_active,
                        message: claim.message.clone(),
                        superseded: false,
                    });
                }
                ledgers
            })
        })
        .collect()
}

pub fn explain_subject_observation(
    subject: &Subject,
    status: Option<&ChangeRequestStatus>,
    landed: bool,
    now: DateTime<Utc>,
    change_request_stale_after: Duration,
) -> ExplainedSubjectObservation {
    let facts = change_request_facts(status, landed).into_iter().collect::<BTreeMap<_, _>>();
    let text = |key| match facts.get(key) {
        Some(MetadataValue::Text(value)) => Some(value.clone()),
        Some(MetadataValue::Bool(value)) => Some(value.to_string()),
        _ => None,
    };
    let field = |key, time| {
        let observed_at = text(time);
        let at = observed_at.as_deref().and_then(|at| DateTime::parse_from_rfc3339(at).ok()).map(|at| at.with_timezone(&Utc));
        ExplainedSubjectFact { value: text(key), observed_at, freshness: observed_freshness(at, now, change_request_stale_after) }
    };
    // Unknown observations retain timestamps and count toward the oldest input age,
    // matching readiness's waiting state for incomplete evidence.
    let readiness_at = status.map(|status| evaluate_change_request_readiness(status, landed).observed_at);
    ExplainedSubjectObservation {
        subject: subject.clone(),
        state: field(KEY_CHANGE_REQUEST_STATE, KEY_CHANGE_REQUEST_STATE_OBSERVED_AT),
        checks: field(KEY_CHANGE_REQUEST_CHECKS, KEY_CHANGE_REQUEST_CHECKS_OBSERVED_AT),
        review: field(KEY_CHANGE_REQUEST_REVIEW_DECISION, KEY_CHANGE_REQUEST_REVIEW_DECISION_OBSERVED_AT),
        review_actionable_at_head: field(
            KEY_CHANGE_REQUEST_REVIEW_ACTIONABLE_AT_HEAD,
            KEY_CHANGE_REQUEST_REVIEW_ACTIONABLE_AT_HEAD_OBSERVED_AT,
        ),
        readiness: ExplainedSubjectFact {
            value: text(KEY_CHANGE_REQUEST_READINESS),
            observed_at: readiness_at.map(|at| at.to_rfc3339()),
            freshness: observed_freshness(readiness_at, now, change_request_stale_after),
        },
    }
}

/// Active declaration attention, with history and other demand categories
/// excluded by declared indexes before decoding.
pub fn attention_query(scope: Option<(&str, &BTreeSet<String>)>) -> StoreQuery {
    let states = [
        StoreQuery { missing_fields: ["status.state".into()].into(), ..Default::default() },
        StoreQuery {
            field_values: BTreeMap::from([("status.state".into(), ["raised".into(), "escalated".into()].into())]),
            ..Default::default()
        },
    ];
    let origins = scope
        .map(|(project, convoys)| {
            vec![
                StoreQuery {
                    fields: BTreeMap::from([
                        ("spec.originating_work_ref.kind".into(), "Project".into()),
                        ("spec.originating_work_ref.name".into(), project.into()),
                    ]),
                    ..Default::default()
                },
                StoreQuery {
                    fields: BTreeMap::from([("spec.originating_work_ref.kind".into(), "Convoy".into())]),
                    field_values: BTreeMap::from([("spec.originating_work_ref.name".into(), convoys.clone().into_iter().collect())]),
                    ..Default::default()
                },
            ]
        })
        .unwrap_or_else(|| vec![StoreQuery::default()]);
    let mut selections = Vec::new();
    for prefix in [DECLARATION_REFUSAL_ATTENTION_PREFIX, ENSURE_DRIFT_ATTENTION_PREFIX] {
        for state in &states {
            for origin in &origins {
                let mut query = origin.clone();
                query.missing_fields.extend(state.missing_fields.clone());
                query.field_values.extend(state.field_values.clone());
                query.field_prefixes.insert("metadata.name".into(), prefix.into());
                selections.push(query);
            }
        }
    }
    StoreQuery { any_of: (!selections.is_empty()).then_some(selections), include_replicas: true, ..Default::default() }
}

/// Gather only the receiver evidence that can affect this convoy. The request
/// snapshot turns the shared receiver resolver's joins into memory operations;
/// it cannot fall back to storage or mutate it.
pub async fn convoy_receiver_evidence(
    loader: &dyn MessageReceiverLoader,
    backend: &ResourceBackend,
    namespace: &str,
    convoys: &[ReadResourceObject<ResourceConvoy>],
    convoy: &ResourceObject<ResourceConvoy>,
) -> Result<(Box<dyn MessageReceiverRead>, Option<ResourceObject<Project>>, BTreeSet<String>), String> {
    use flotilla_resources::{ConvoyEnsure, CrewDefaults, FleetDesignation, FLEET_DESIGNATION_NAME, ROLE_ADDRESS_ANNOTATION};
    let project_name = convoy.spec.project_ref.as_deref().unwrap_or(namespace);
    let target = StoreQuery::labels(&BTreeMap::from([(CONVOY_LABEL.into(), convoy.metadata.name.clone())]), true, true);
    let terminals = backend
        .query_declared::<ResourceTerminalSession>(
            namespace,
            &StoreQuery {
                any_of: Some(vec![
                    target.clone(),
                    StoreQuery::related(
                        "metadata.annotations.\"flotilla.work/role-address\"",
                        "metadata.annotations.\"flotilla.work/role-address\"",
                        target,
                    ),
                ]),
                include_replicas: true,
                ..Default::default()
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    let ensures = backend
        .query_declared::<ConvoyEnsure>(
            namespace,
            &StoreQuery {
                fields: BTreeMap::from([("spec.project_ref".into(), project_name.into())]),
                include_replicas: true,
                ..StoreQuery::related(
                    "spec.role",
                    "spec.role",
                    StoreQuery {
                        fields: BTreeMap::from([
                            ("status.convoy_ref".into(), convoy.metadata.name.clone()),
                            ("spec.project_ref".into(), project_name.into()),
                        ]),
                        include_replicas: true,
                        ..Default::default()
                    },
                )
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    let mut aliases = ensures
        .items
        .iter()
        .filter(|ensure| ensure.object.status.as_ref().and_then(|status| status.convoy_ref.as_deref()) == Some(&convoy.metadata.name))
        .map(|ensure| format!("{}/{}", ensure.object.spec.project_ref, ensure.object.spec.role))
        .collect::<BTreeSet<_>>();
    aliases.extend(
        terminals
            .items
            .iter()
            .filter(|terminal| terminal.object.metadata.labels.get(CONVOY_LABEL) == Some(&convoy.metadata.name))
            .filter_map(|terminal| terminal.object.metadata.annotations.get(ROLE_ADDRESS_ANNOTATION).cloned()),
    );
    let adopted = terminals.items.iter().any(|terminal| terminal.object.metadata.annotations.contains_key(ROLE_ADDRESS_ANNOTATION));
    // Every declaration can participate in hierarchy validation; this catalog
    // is read once and reused for the reference projection and receiver cascade.
    let projects = if adopted {
        project_ops::read_definition_catalog::<Project>(backend, namespace).await?
    } else {
        match &convoy.spec.project_ref {
            Some(name) => backend.definitions::<Project>(namespace).list_names([name.clone()]).await.map_err(|error| error.to_string())?,
            None => Vec::new(),
        }
    };
    let project = projects.iter().find(|project| project.metadata.name == project_name).cloned();
    let mut snapshot = project_ops::RequestSnapshot::new(backend, namespace)?;
    snapshot.insert(ensures)?;
    snapshot.insert_definitions(projects)?;
    if adopted {
        let fleet = backend
            .definitions::<FleetDesignation>(namespace)
            .list_names([FLEET_DESIGNATION_NAME.into()])
            .await
            .map_err(|error| error.to_string())?;
        snapshot.insert_definitions(fleet)?;
        // All defaults are cascade layers; their order and refusals participate.
        let defaults = project_ops::read_definition_catalog::<CrewDefaults>(backend, namespace).await?;
        snapshot.insert_definitions(defaults)?;
    }
    let receivers = loader.load(&snapshot.finish(), namespace, convoys.to_vec(), terminals.items).await?;
    Ok((receivers, project, aliases))
}

/// Select the active inbox and receiver candidates before decoding; resolve
/// their holders purely against the request's shared evidence.
pub async fn message_views(
    backend: &flotilla_store::ResourceBackend,
    namespace: &str,
    receivers: &dyn MessageReceiverRead,
    convoy: Option<&flotilla_resources::ResourceObject<ResourceConvoy>>,
    aliases: &BTreeSet<String>,
) -> Result<Vec<flotilla_protocol::query::CrewMessageView>, String> {
    Ok(message_evidence(backend, namespace, receivers, convoy, aliases).await?.0)
}

/// [`message_views`], with the held batches among the same Messages (#3455).
pub async fn message_evidence(
    backend: &flotilla_store::ResourceBackend,
    namespace: &str,
    receivers: &dyn MessageReceiverRead,
    convoy: Option<&flotilla_resources::ResourceObject<ResourceConvoy>>,
    aliases: &BTreeSet<String>,
) -> Result<(Vec<flotilla_protocol::query::CrewMessageView>, Vec<flotilla_resources::HeldMessageBatch>), String> {
    let any_of = convoy
        .map(|convoy| {
            vec![
                StoreQuery {
                    field_prefixes: BTreeMap::from([(
                        "spec.receiver".into(),
                        format!("{}/{}/", convoy.spec.project_ref.as_deref().unwrap_or(namespace), convoy.metadata.name),
                    )]),
                    ..Default::default()
                },
                StoreQuery {
                    field_values: BTreeMap::from([("spec.receiver".into(), aliases.clone().into_iter().collect())]),
                    ..Default::default()
                },
            ]
        })
        .unwrap_or_default();
    let mut records = match receivers.messages() {
        Some(messages) => messages
            .iter()
            .filter(|record| record.object.status.as_ref().is_none_or(|status| !status.phase.is_terminal()))
            .filter(|record| {
                convoy.is_none_or(|convoy| {
                    let prefix = format!("{}/{}/", convoy.spec.project_ref.as_deref().unwrap_or(namespace), convoy.metadata.name);
                    record.object.spec.receiver.starts_with(&prefix) || aliases.contains(&record.object.spec.receiver)
                })
            })
            .cloned()
            .collect(),
        None => {
            backend
                .query_declared::<flotilla_resources::Message>(
                    namespace,
                    &StoreQuery {
                        message: Some(flotilla_store::MessageQuery::active()),
                        any_of: (!any_of.is_empty()).then_some(any_of),
                        include_replicas: true,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|error| error.to_string())?
                .items
        }
    };
    if let Some(convoy) = convoy {
        records.retain(|record| {
            receivers.may_address_convoy(
                &record.object.spec.receiver,
                convoy.spec.project_ref.as_deref().unwrap_or(namespace),
                &convoy.metadata.name,
            )
        });
    }
    records.sort_by_key(|record| {
        (record.object.metadata.creation_timestamp, record.object.status.as_ref().and_then(|status| status.accepted_sequence))
    });
    let mut views = Vec::new();
    let mut shown = Vec::new();
    for source in records {
        let message = source.object;
        // A diagnostic view shows an unresolvable receiver beside the Message
        // instead of failing the whole read; delivery keeps refusing it (#3009).
        let (holder, receiver_error) = match receivers.resolve(&message.spec.receiver) {
            Ok(holder) => (holder, None),
            Err(error) => (None, Some(error)),
        };
        let current_receiver = holder.and_then(|holder| {
            let terminal = holder.object;
            let convoy = terminal.metadata.labels.get(flotilla_resources::CONVOY_LABEL)?;
            let vessel = terminal.metadata.labels.get(flotilla_resources::VESSEL_LABEL)?;
            Some(format!("{}/{convoy}/{vessel}/{}", message.spec.receiver.split('/').next()?, terminal.spec.role))
        });
        if let Some(convoy) = convoy {
            let direct = message.spec.receiver.split('/').collect::<Vec<_>>();
            let is_direct = matches!(direct.as_slice(), [_, name, _, _] if *name == convoy.metadata.name);
            // An unresolvable receiver has no holder to confirm the convoy, so
            // keep it on the evidence `may_address_convoy` already required: a
            // four-part address naming this convoy, or a role declaration or
            // terminal claim pointing at it. Hiding it would hide the ambiguity.
            if !is_direct
                && receiver_error.is_none()
                && !current_receiver.as_deref().is_some_and(|receiver| receiver.split('/').nth(1) == Some(convoy.metadata.name.as_str()))
            {
                continue;
            }
        }
        shown.push(message.clone());
        let status = message.status.unwrap_or_default();
        let receiver = status.resolved_receiver.as_ref();
        views.push(flotilla_protocol::query::CrewMessageView {
            current_receiver,
            receiver_error,
            name: message.metadata.name,
            sender: message.spec.sender,
            receiver: message.spec.receiver,
            relation: serde_json::to_value(message.spec.relation).expect("relation").as_str().expect("relation string").into(),
            phase: serde_json::to_value(status.phase).expect("phase").as_str().expect("phase string").into(),
            since: status.since.to_rfc3339(),
            reason: status.reason,
            subject: message.spec.subject.map(|subject| serde_json::to_value(subject).expect("subject")),
            expectation: serde_json::to_value(message.spec.expectation).expect("expectation"),
            crew_id: receiver.map(|receiver| receiver.crew_id.clone()),
            session: receiver.map(|receiver| receiver.session.clone()),
        });
    }
    Ok((views, flotilla_resources::HeldMessageBatch::collect(&shown)))
}

/// The record name a change-request record's spec addresses, resolving a
/// host-spelled service through `forges`.
fn spec_record_name(spec: &flotilla_resources::ChangeRequestSpec, forges: &[flotilla_resources::ForgeSpec]) -> Option<String> {
    spec.address(forges).ok().and_then(|address| record_name(&address))
}

/// Read only the hosts that can affect this convoy's inboxes.
async fn message_controller_warnings(backend: &ResourceBackend, namespace: &str, hosts: BTreeSet<String>) -> Result<Vec<String>, String> {
    let sources =
        backend.query_declared::<ResourceHost>(namespace, &StoreQuery::names(hosts, true)).await.map_err(|error| error.to_string())?;
    Ok(sources
        .items
        .into_iter()
        .filter_map(|host| {
            let condition = host.object.status.as_ref().and_then(message_controller_failure)?;
            Some(format!("host {}: {} ({}): {}", host.object.metadata.name, condition.condition_type, condition.reason, condition.message))
        })
        .collect())
}

#[cfg(test)]
mod message_health_tests {
    use super::*;

    // Explain names the stopped Message controller only on an affected host,
    // and clears the warning when that host's durable condition clears.
    #[tokio::test]
    async fn explanation_names_message_controller_exhaustion_and_recovery() {
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        let hosts = backend.using::<ResourceHost>("flotilla");
        let host = hosts
            .create(&flotilla_resources::InputMeta::builder().name("feta".into()).build(), &flotilla_resources::HostSpec::default())
            .await
            .unwrap();
        let status = ResourceHostStatus {
            conditions: vec![flotilla_resources::HostCondition::builder()
                .condition_type("Controller/Message")
                .value(ConditionValue::False)
                .reason("RestartBudgetExhausted")
                .message("Message controller stopped after 10 failures")
                .observed_at(Utc::now())
                .blocks_forge_observation(false)
                .build()],
            ..Default::default()
        };
        let host = hosts.update_status("feta", &host.metadata.resource_version, &status).await.unwrap();
        assert!(message_controller_warnings(&backend, "flotilla", ["other-host".into()].into()).await.unwrap().is_empty());
        let warnings = message_controller_warnings(&backend, "flotilla", ["feta".into()].into()).await.unwrap();
        assert_eq!(warnings, ["host feta: Controller/Message (RestartBudgetExhausted): Message controller stopped after 10 failures"]);
        hosts.update_status("feta", &host.metadata.resource_version, &ResourceHostStatus::default()).await.unwrap();
        assert!(message_controller_warnings(&backend, "flotilla", ["feta".into()].into()).await.unwrap().is_empty());
    }
}
