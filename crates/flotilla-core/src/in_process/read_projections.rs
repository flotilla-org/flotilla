//! Read-side query projections over resource state and explicit runtime inputs.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use chrono::{DateTime, Utc};
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
    HostStatusResponse, HostSummary, NodeId, PeerConnectionState, ProjectListEntry, ProjectListRepository, ProjectListResponse, Subject,
    SubjectKind, ViewAddress,
};
use flotilla_resources::{
    bound_change_request_record_name, convoy_subject_rows, evaluate_landing_settlement, expected_change_request_leaves,
    expected_checkout_refs, repository_display_labels, resolve_project_issue_sources, ChangeRequestStatus, Checkout as ResourceCheckout,
    Clock, ConditionValue, Convoy as ResourceConvoy, ConvoyStatus, CrewMessageSender, CrewWorkPhase, Demand as ResourceDemand, DemandState,
    EventRecorder, Forge, FulfilmentGrant, FulfilmentKind, FulfilmentRealisation, Host as ResourceHost, HostStatus as ResourceHostStatus,
    IssueSourceResolution, IssueSourceUnavailable, ManifestRoot, Project, ReadResourceObject, Repository, RepositoryKey, ResourceBackend,
    ResourceObject, ResourceProvenance, SettlementMode, TerminalAttentionState, TerminalSession as ResourceTerminalSession,
    TerminalSessionPhase as ResourceTerminalSessionPhase, TerminalSessionSource, Vessel, WorkPhase as ResourceWorkPhase, WorkflowTemplate,
    CONVOY_LABEL, HEARTBEAT_READY_TTL_SECS, PROJECT_LABEL, ROLE_LABEL, VESSEL_LABEL,
};
use tracing::warn;

use super::{resolve_convoy_candidate_indices, ConvoyAddressIdentity};
use crate::{
    checkout_integration::LANDING_EVIDENCE_TTL,
    config::ConfigStore,
    environment_manager::EnvironmentManager,
    event_sink::EventSink,
    fleet::{
        accumulate_fleet_health_counts, fleet_observation_agreement, format_resource_replication_failures, host_credential_attention,
        join_replica_errors, replica_sync_is_fresh, replicated_host_reports, FleetService, ResourceReplicationFailure,
    },
    host_registry::HostCounts,
    leaf_engine::{LeafSubscriptionTable, LeafWatcher},
    ops_entry::{
        DECLARATION_REFUSAL_ATTENTION_PREFIX, DECLARATION_REFUSAL_REASON_ANNOTATION, DECLARATION_REFUSED_SINCE_ANNOTATION,
        DECLARATION_STALE_AFTER, ENSURE_CONFIG_DRIFT_REASON_ANNOTATION, ENSURE_DRIFT_ATTENTION_PREFIX,
    },
    resource_explain::{explain_condition, explain_unmet_expectation, explained_provenance, observed_freshness},
};

/// Projects resource and fleet state supplied by the daemon and FleetService.
/// Host refresh stays with the daemon; FleetService gathers local and replica rows.
pub(super) struct ReadProjections<'a> {
    // Wired now for read-side operations introduced by later slices.
    pub(super) _event_sink: Arc<dyn EventSink>,
    pub(super) backend: &'a ResourceBackend,
    pub(super) config: &'a ConfigStore,
    pub(super) host_registry: &'a crate::host_registry::HostRegistry,
    pub(super) environment_manager: &'a EnvironmentManager,
    pub(super) host_name: &'a HostName,
    pub(super) node_id: &'a NodeId,
    pub(super) clock: &'a Arc<dyn Clock>,
    pub(super) leaf_subscriptions: &'a LeafSubscriptionTable,
    pub(super) fleet: &'a FleetService,
}

impl ReadProjections<'_> {
    /// Read local and replicated convoy conditions across every stored namespace.
    pub(super) async fn crew_stalls(
        backend: &ResourceBackend,
        full: bool,
        now: DateTime<Utc>,
    ) -> Result<flotilla_protocol::CrewStallsResponse, String> {
        let mut rows = Vec::new();
        for namespace in backend.stored_namespaces::<ResourceConvoy>().await.map_err(|error| error.to_string())? {
            let projects = backend.definitions::<Project>(&namespace).list().await.map_err(|error| error.to_string())?;
            let artifacts =
                backend.including_replicas::<flotilla_resources::Artifact>(&namespace).list().await.map_err(|error| error.to_string())?;
            for source in backend.including_replicas::<ResourceConvoy>(&namespace).list().await.map_err(|error| error.to_string())?.items {
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

    pub(super) async fn list_hosts(&self, counts: &HashMap<EnvironmentId, HostCounts>) -> Result<HostListResponse, String> {
        Ok(self.host_registry.list_hosts(counts).await)
    }

    pub(super) async fn dispatch_queue(
        backend: &ResourceBackend,
        namespace: &str,
        project_filter: Option<&str>,
        observed_at: DateTime<Utc>,
    ) -> Result<DispatchQueueResponse, String> {
        let projects = backend.clone().definitions::<Project>(namespace).list().await.map_err(|error| error.to_string())?;
        let mut entries = Vec::new();
        for project in projects {
            if project_filter.is_some_and(|filter| filter != project.metadata.name) {
                continue;
            }
            let Some(status) = project.status else { continue };
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
                        .provenance(entry.provenance)
                        .build(),
                );
            }
        }
        entries.sort_by(|left, right| {
            (&left.namespace, &left.project, left.ready_observed_at, &left.issue).cmp(&(
                &right.namespace,
                &right.project,
                right.ready_observed_at,
                &right.issue,
            ))
        });
        Ok(DispatchQueueResponse { observed_at, entries })
    }

    pub(super) async fn fulfilment_list(&self, namespace: &str) -> Result<FulfilmentListResponse, String> {
        let hosts = self.backend.clone().including_replicas::<ResourceHost>(namespace).list().await.map_err(|error| error.to_string())?;
        let mut facts_by_host = BTreeMap::<String, ResourceHostStatus>::new();
        for host in hosts.items {
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
        let kinds = self.backend.clone().including_replicas::<FulfilmentKind>(namespace).list().await.map_err(|error| error.to_string())?;
        let mut rows = BTreeMap::new();
        for kind in kinds.items {
            let kind = kind.object;
            if kind.metadata.deletion_timestamp.is_some() {
                continue;
            }
            let facts = facts_by_host.get(&kind.spec.host_ref).and_then(|status| status.fulfilment_facts.get(&kind.metadata.name));
            let grants = kind
                .spec
                .grants
                .iter()
                .map(|grant| match grant {
                    FulfilmentGrant::Platform(value) => format!("platform:{value}"),
                    FulfilmentGrant::GuiSession => "gui_session".to_string(),
                    FulfilmentGrant::Gpu => "gpu".to_string(),
                    FulfilmentGrant::HostDevices => "host_devices".to_string(),
                    FulfilmentGrant::Network(value) => format!("network:{value}"),
                    FulfilmentGrant::HostAccountReach => "host_account_reach".to_string(),
                    FulfilmentGrant::ContainerRuntime => "container_runtime".to_string(),
                    FulfilmentGrant::Toolchain(value) => format!("toolchain:{value}"),
                })
                .collect();
            let harnesses = facts
                .map(|facts| {
                    facts
                        .harnesses
                        .iter()
                        .map(|(name, harness)| {
                            let models = harness
                                .models
                                .iter()
                                .map(|(name, model)| {
                                    (name.clone(), FulfilmentModel {
                                        usable: model.usable,
                                        source: match model.source {
                                            flotilla_resources::ModelFactSource::Probe => "probe",
                                            flotilla_resources::ModelFactSource::Declaration => "declaration",
                                        }
                                        .to_string(),
                                    })
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
                }
                .to_string(),
                grants,
                harnesses,
                toolchains: facts.map(|facts| facts.toolchains.clone()).unwrap_or_default(),
                gui_session_logged_in: facts.map(|facts| facts.gui_session_logged_in),
                free_vessel_slots: facts.and_then(|facts| facts.free_vessel_slots),
                image: facts.and_then(|facts| facts.image.clone()),
            };
            rows.insert((row.host_ref.clone(), row.name.clone()), row);
        }
        Ok(FulfilmentListResponse { kinds: rows.into_values().collect() })
    }

    pub(super) async fn fleet_health(
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
        let resource_hosts =
            self.backend.clone().including_replicas::<ResourceHost>(namespace).list().await.map_err(|error| error.to_string())?;
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
        let manifest_roots =
            self.backend.clone().including_replicas::<ManifestRoot>(namespace).list().await.map_err(|error| error.to_string())?;
        for root in manifest_roots.items {
            let Some(host) = host_refs.get(&root.object.spec.host) else { continue };
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
                        + manifest_needs_by_host.get(&host).copied().unwrap_or_default(),
                };
                let degraded_conditions = status
                    .into_iter()
                    .flat_map(|status| status.conditions.iter())
                    .filter(|condition| condition.value == ConditionValue::False)
                    .map(|condition| format!("{}: {}", condition.condition_type, condition.message))
                    .collect();
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
        Ok(FleetHealthResponse { hosts: rows, dispatch_queue })
    }

    pub(super) async fn list_projects(
        backend: &ResourceBackend,
        namespace: &str,
        now: DateTime<Utc>,
    ) -> Result<ProjectListResponse, String> {
        let projects = backend.clone().definitions::<Project>(namespace).list().await.map_err(|error| error.to_string())?;
        let repositories = backend.clone().using::<Repository>(namespace).list().await.map_err(|error| error.to_string())?;
        let repositories = repositories
            .items
            .into_iter()
            .map(|repository| (RepositoryKey(repository.metadata.name.clone()), repository))
            .collect::<Vec<_>>();
        let repository_slugs = repository_display_labels(repositories.iter().map(|(key, repository)| (key, &repository.spec)));

        let mut entries = Vec::new();
        for project in projects {
            let issue_sources =
                match resolve_project_issue_sources(&backend.including_replicas::<Repository>(namespace), &project.spec).await {
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

    pub(super) async fn host_statuses(&self, namespace: &str) -> Result<BTreeMap<String, ResourceHostStatus>, String> {
        let hosts = self
            .backend
            .clone()
            .including_replicas::<ResourceHost>(namespace)
            .list_replica_sources()
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

    pub(super) async fn get_host_status(
        &self,
        environment_id: &EnvironmentId,
        counts: &HashMap<EnvironmentId, HostCounts>,
        local_summary: &HostSummary,
    ) -> Result<HostStatusResponse, String> {
        let mut response = self.host_registry.get_host_status(environment_id, counts).await?;
        if environment_id == &local_summary.environment_id && !self.host_registry.has_resource_description(environment_id).await {
            response.visible_environments = self.environment_manager.visible_environments().await;
        }
        Ok(response)
    }

    pub(super) async fn get_host_providers(
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

    pub(super) async fn fleet_list(
        &self,
        namespace: &str,
        mut rows: Vec<FleetListRow>,
        now: DateTime<Utc>,
    ) -> Result<FleetListResponse, String> {
        let mut replicas = Vec::new();
        let host_sources =
            self.backend.clone().including_replicas::<ResourceHost>(namespace).list().await.map_err(|error| error.to_string())?;
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
            replicas.push(FleetReplicaStatus { host, reachable, last_sync, generation, message });
        }
        for (host, (last_sync, generation)) in replicated_hosts {
            let replication_error =
                replication_failures_by_host.remove(&host).and_then(|failures| format_resource_replication_failures(&failures));
            replicas.push(FleetReplicaStatus {
                host,
                reachable: replica_sync_is_fresh(last_sync, now) && replication_error.is_none(),
                last_sync: Some(last_sync),
                generation,
                message: replication_error,
            });
        }
        for (host, failures) in replication_failures_by_host {
            replicas.push(FleetReplicaStatus {
                host,
                reachable: false,
                last_sync: None,
                generation: None,
                message: format_resource_replication_failures(&failures),
            });
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
        for source in self.backend.including_replicas::<ResourceDemand>(namespace).list().await.map_err(|error| error.to_string())?.items {
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
        Ok(FleetListResponse { rows, replicas, declaration_attention })
    }

    pub(super) async fn scoped_fleet_list(
        &self,
        namespace: &str,
        mut fleet: FleetListResponse,
        project: Option<&str>,
        crew_id: Option<&str>,
        convoy: Option<&str>,
    ) -> Result<FleetListResponse, String> {
        if project.is_none() && crew_id.is_none() && convoy.is_none() {
            return Ok(fleet);
        }
        let convoys = self.backend.including_replicas::<ResourceConvoy>(namespace).list().await.map_err(|err| err.to_string())?;
        let context_convoy = if project.is_some() {
            None
        } else if let Some(crew_id) = crew_id {
            let sessions =
                self.backend.including_replicas::<ResourceTerminalSession>(namespace).list().await.map_err(|err| err.to_string())?;
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
            convoy.map(ToOwned::to_owned)
        };
        let selected_project = match project {
            Some(project) => project.to_string(),
            None => {
                let convoy = context_convoy.expect("scope requires a convoy");
                convoys
                    .items
                    .iter()
                    .find(|source| source.object.metadata.name == convoy)
                    .ok_or_else(|| format!("crew convoy `{convoy}` not found"))?
                    .object
                    .spec
                    .project_ref
                    .clone()
                    .ok_or_else(|| format!("crew convoy `{convoy}` has no project"))?
            }
        };
        let matching: HashSet<_> = convoys
            .items
            .iter()
            .filter(|source| source.object.spec.project_ref.as_deref() == Some(selected_project.as_str()))
            .map(|source| source.object.metadata.name.as_str())
            .collect();
        fleet.rows.retain(|row| row.convoy_ref.as_deref().is_some_and(|reference| matching.contains(reference)));
        fleet.declaration_attention.retain(|row| match row.resource.kind.as_str() {
            "Project" => row.resource.name == selected_project,
            "Convoy" => matching.contains(row.resource.name.as_str()),
            _ => false,
        });
        fleet.replicas.clear();
        Ok(fleet)
    }

    pub(super) async fn explain_convoy(&self, namespace: &str, name: &str) -> Result<ConvoyExplanation, String> {
        let convoy_sources =
            self.backend.including_replicas::<ResourceConvoy>(namespace).list().await.map_err(|error| error.to_string())?;
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
        let convoy = convoy_source.object.clone();
        let now = self.clock.now();
        let forges = self
            .backend
            .definitions::<Forge>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|forge| forge.spec)
            .collect::<Vec<_>>();
        let reference_context = flotilla_protocol::ReferenceContext {
            repositories: convoy
                .spec
                .repositories
                .iter()
                .filter_map(|repository| {
                    let address = flotilla_resources::change_request_address_with_forges(&repository.url, "1", &forges).ok()?;
                    let flotilla_protocol::LeafAddress::ChangeRequest { service, scope, .. } = address else { return None };
                    let canonical = flotilla_resources::canonicalize_repo_url(&repository.url).ok()?;
                    let web_base = forges
                        .iter()
                        .find(|forge| forge.forge_id == service)
                        .map(|forge| forge.https_url.clone())
                        .or_else(|| canonical.strip_suffix(&format!("/{scope}")).map(str::to_string))?;
                    let forge_alias = (service != "github.com").then(|| service.clone());
                    Some(flotilla_protocol::RepositoryAlias {
                        project: convoy.spec.project_ref.clone(),
                        alias: scope.rsplit('/').next()?.to_string(),
                        source: flotilla_protocol::IssueSource { service, scope },
                        web_base,
                        forge_alias,
                    })
                })
                .collect(),
        };
        let subjects = convoy_subject_rows(&convoy, &reference_context);
        let change_request_stale_after = self.leaf_subscriptions.change_request_stale_after();

        let checkout_sources =
            self.backend.including_replicas::<ResourceCheckout>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let selected_checkouts = flotilla_resources::select_convoy_children(&convoy, &checkout_sources);
        let vessel_sources = self.backend.including_replicas::<Vessel>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let selected_vessels = flotilla_resources::select_convoy_children(&convoy, &vessel_sources);
        let expected = expected_checkout_refs(&convoy).map_err(|error| format!("derive expected checkouts: {error}"))?;
        let checkouts = expected
            .iter()
            .map(|checkout_name| {
                let selected = selected_checkouts.get(checkout_name);
                let provenance = selected.and_then(|object| {
                    checkout_sources
                        .iter()
                        .find(|source| {
                            source.object.metadata.name == object.metadata.name
                                && source.object.metadata.resource_version == object.metadata.resource_version
                        })
                        .map(|source| explained_provenance(&source.provenance, self.node_id))
                });
                let integration = selected.and_then(|checkout| checkout.status.as_ref()).map(|status| &status.integration);
                ExplainedCheckout {
                    name: checkout_name.clone(),
                    observed: selected.is_some(),
                    provenance,
                    clean: integration.map(|status| explain_condition(&status.clean, now, LANDING_EVIDENCE_TTL)),
                    pushed: integration.map(|status| explain_condition(&status.pushed, now, LANDING_EVIDENCE_TTL)),
                    landed: integration.map(|status| explain_condition(&status.landed, now, LANDING_EVIDENCE_TTL)),
                }
            })
            .collect::<Vec<_>>();

        let change_request_sources = self
            .backend
            .including_replicas::<flotilla_resources::ChangeRequest>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items;
        let selected_change_requests =
            flotilla_resources::select_change_request_sources(change_request_sources.iter().map(|source| (&source.object, source)));
        let change_request_objects = selected_change_requests
            .values()
            .map(|(object, _)| {
                (
                    flotilla_resources::change_request_record_name(&object.spec.service, &object.spec.scope, object.spec.number),
                    object.clone(),
                )
            })
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
        let expected_change_request_leaves = expected_change_request_leaves(&convoy, &selected_checkouts)
            .map_err(|error| format!("derive expected change requests: {error}"))?;
        let expected_change_requests = expected_change_request_leaves
            .iter()
            .filter_map(|leaf| match &leaf.address {
                flotilla_protocol::LeafAddress::ChangeRequest { service, scope, number } => {
                    Some(flotilla_resources::change_request_record_name(service, scope, *number))
                }
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let bound_name = bound_change_request_record_name(&convoy).map_err(|error| format!("derive bound change request: {error}"))?;
        let mut observation_errors = BTreeMap::new();
        for leaf in expected_change_request_leaves {
            if let flotilla_protocol::LeafAddress::ChangeRequest { service, scope, number } = leaf.address {
                let subject = crate::change_request_observer::ChangeRequestRef { namespace: namespace.to_string(), service, scope, number };
                if let Some(error) = self.leaf_subscriptions.change_request_observation_error(&subject).await {
                    observation_errors.insert(subject.record_name(), error.to_string());
                }
            }
        }
        let change_requests = expected_change_requests
            .iter()
            .map(|record_name| {
                let selected = change_request_objects.get(record_name);
                let provenance = selected_change_requests
                    .values()
                    .find(|(object, _)| {
                        flotilla_resources::change_request_record_name(&object.spec.service, &object.spec.scope, object.spec.number)
                            == *record_name
                    })
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

        let terminal_sessions =
            self.backend.including_replicas::<ResourceTerminalSession>(namespace).list().await.map_err(|error| error.to_string())?.items;
        let unclaimed_work = explained_unclaimed_work(convoy.status.as_ref(), &terminal_sessions, name);
        let mut crew_deliveries = terminal_sessions
            .into_iter()
            .filter(|source| source.object.metadata.labels.get(CONVOY_LABEL).is_some_and(|convoy| convoy == name))
            .map(|source| ExplainedCrewDelivery {
                terminal_condition: source.object.status.as_ref().and_then(crate::terminal_health::condition),
                session: source.object.metadata.name,
                role: source.object.spec.role,
                // ADR 0028's delivery ladder has not landed yet. Keep the
                // field explicit so recorded rungs appear without inventing
                // one from session liveness or message delivery.
                last_delivery_rung: None,
                sender: match &source.object.spec.source {
                    TerminalSessionSource::Agent { message: Some(message), .. } if message.sender != CrewMessageSender::Unknown => {
                        Some(message.sender.clone())
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

        let decision_ledgers = explained_decision_ledgers(convoy.status.as_ref());
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
        let pinned_workflow =
            self.backend.including_replicas::<WorkflowTemplate>(namespace).get(flotilla_resources::pinned_workflow_ref(&convoy)).await.ok();
        let role_vessels = convoy
            .status
            .as_ref()
            .and_then(|status| status.workflow_snapshot.as_ref())
            .map(|snapshot| snapshot.vessels.clone())
            .or_else(|| pinned_workflow.as_ref().map(|workflow| workflow.object.spec.vessels.clone()))
            .unwrap_or_default();

        let stores = self.config.load_daemon_config()?.blob_stores;
        let mut artifacts = self
            .backend
            .including_replicas::<flotilla_resources::Artifact>(namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .filter(|item| item.object.spec.convoy == convoy.metadata.name)
            .map(|item| ExplainedArtifact {
                kind: item.object.spec.kind.clone(),
                address: format!("artifact/{}", item.object.metadata.name),
                view_url: crate::config::artifact_view_url(&item.object.spec, &stores),
            })
            .collect::<Vec<_>>();
        artifacts.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.address.cmp(&b.address)));

        Ok(ConvoyExplanation {
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
            unclaimed_work,
            decision_ledgers,
            artifacts,
            settlement,
            recent_events,
            lifecycle_mutations,
        })
    }
}

pub(super) fn credential_refresh_alert_for_vessel(demand: &ResourceObject<ResourceDemand>, convoy: &str, vessel: &str) -> Option<String> {
    if demand.spec.originating_work_ref.name != convoy
        || demand.metadata.annotations.get("flotilla.work/credential-refresh-vessel").is_none_or(|name| name != vessel)
        || !matches!(
            demand.status.as_ref().map_or(DemandState::Raised, |status| status.state),
            DemandState::Raised | DemandState::Escalated
        )
    {
        return None;
    }
    demand.metadata.annotations.get("flotilla.work/credential-refresh-reason").cloned()
}

fn explained_unclaimed_work(
    status: Option<&ConvoyStatus>,
    sessions: &[ReadResourceObject<ResourceTerminalSession>],
    convoy_name: &str,
) -> Vec<ExplainedUnclaimedWork> {
    let Some(status) = status else { return Vec::new() };
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

fn explained_decision_ledgers(status: Option<&ConvoyStatus>) -> Vec<ExplainedDecisionLedger> {
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
                        comment_url: superseded.decision_ledger_ref.clone(),
                        missing: superseded.decision_ledger_ref.is_none(),
                        override_principal: superseded.completion_override.as_ref().map(|override_| override_.principal.clone()),
                        completed_while_crew_active: superseded.completed_while_crew_active,
                        message: superseded.message.clone(),
                        superseded: true,
                    })
                    .collect::<Vec<_>>();
                if matches!(claim.phase, CrewWorkPhase::Done | CrewWorkPhase::HandedBack) {
                    ledgers.push(ExplainedDecisionLedger {
                        vessel: vessel.clone(),
                        role: role.clone(),
                        claimed_at: claim.finished_at.map(|at| at.to_rfc3339()),
                        comment_url: claim.decision_ledger_ref.clone(),
                        missing: claim.decision_ledger_ref.is_none(),
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

fn explain_subject_observation(
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

#[cfg(test)]
mod tests {
    use chrono::{Duration as ChronoDuration, TimeZone};
    use flotilla_protocol::{qualified_path::HostId, EvidenceFreshness, HostSummary, IssueSource, NodeInfo, Relationship, SystemInfo};
    use flotilla_resources::{
        ChangeRequest, ChangeRequestReviewObservation, ChangeRequestSpec, ConvoyPhase, ConvoySpec, CrewWorkState, DeclaredSubject,
        DemandKind, DemandSpec, DispatchQueueEntry, FulfilmentKindSpec, HostSpec, InMemoryBackend, InputMeta, Observation,
        ObservedChangeRequestState, ObservedChecks, ObservedMergeability, ObservedReviewDecision, ProjectSpec, ProjectStatus, SystemClock,
        TerminalSessionSource, TerminalSessionSpec,
    };
    use hegel::generators as gs;

    use super::*;
    use crate::{
        aggregator_projection::AggregatorProjectionState,
        change_request_observer::{ChangeRequestRefreshCadence, ChangeRequestRefresher, GhChangeRequestObservationSource},
        providers::{discovery::EnvironmentBag, ProcessCommandRunner},
    };

    // #2498: every distinct stalled actor is visible across namespaces and replica stores.
    // Generate empty through multi-convoy fleets, duplicate leaves, all rungs, shared and distinct
    // evidence, future timestamps, and terminal convoys. No transport interleavings affect this read.
    #[hegel::test]
    fn crew_stalls_cover_each_obligation(tc: hegel::TestCase) {
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let roles = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
        let unrecognized = tc.draw(gs::booleans());
        let shared = tc.draw(gs::booleans());
        let future = tc.draw(gs::booleans());
        let replicated = tc.draw(gs::booleans());
        let rung = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let backend = ResourceBackend::InMemory(InMemoryBackend::default());
            let author = if replicated { ResourceBackend::InMemory(InMemoryBackend::default()) } else { backend.clone() };
            let now = Utc::now();
            let began_at = now + ChronoDuration::seconds(if future { 10 } else { -300 });
            for index in 0..count {
                let namespace = format!("namespace-{index}");
                backend
                    .using::<Project>(&namespace)
                    .create(
                        &InputMeta::builder().name("project".into()).build(),
                        &ProjectSpec::builder().display_name("Project display".into()).default_workflow_ref("scratch".into()).build(),
                    )
                    .await
                    .expect("project");
                let convoys = author.using::<ResourceConvoy>(&namespace);
                for terminal in [false, true] {
                    let convoy = convoys
                        .create(
                            &InputMeta::builder().name(if terminal { "terminal" } else { "stalled" }.into()).build(),
                            &ConvoySpec::builder()
                                .workflow_ref("scratch".into())
                                .role("implement".into())
                                .generation(2)
                                .project_ref("project".into())
                                .build(),
                        )
                        .await
                        .expect("convoy");
                    let leaves: Vec<_> = (0..roles)
                        .flat_map(|role| {
                            let leaf = flotilla_protocol::Leaf {
                                address: flotilla_protocol::LeafAddress::Work { convoy: convoy.metadata.name.clone(), work: "work".into() },
                                field_path: if unrecognized { ".phase".into() } else { format!(".crew.role-{role}.phase") },
                                operator: flotilla_protocol::LeafOperator::Equal,
                                literal: "Done".into(),
                            };
                            [leaf.clone(), leaf]
                        })
                        .collect();
                    convoys
                        .update_status(&convoy.metadata.name, &convoy.metadata.resource_version, &ConvoyStatus {
                            phase: if terminal { ConvoyPhase::Landed } else { ConvoyPhase::Active },
                            stalled: Some(flotilla_resources::StalledCondition {
                                leaves,
                                maker: None,
                                evidence: if shared {
                                    if index % 2 == 0 {
                                        "rate limit".into()
                                    } else {
                                        " rate\n limit ".into()
                                    }
                                } else {
                                    format!("cause {index}")
                                },
                                source: flotilla_resources::StallEvidenceSource::Crew,
                                cause: None,
                                began_at,
                                rung: [
                                    flotilla_resources::StallRung::Nudge,
                                    flotilla_resources::StallRung::Supervisor,
                                    flotilla_resources::StallRung::Bosun,
                                    flotilla_resources::StallRung::Governor,
                                    flotilla_resources::StallRung::Operator,
                                ][rung],
                                supervisor: (rung != 4).then(|| flotilla_resources::StallSupervisor {
                                    convoy: "governor".into(),
                                    vessel: "control".into(),
                                    role: "governor".into(),
                                }),
                                supervision_index: None,
                                supervision_exhausted: false,
                                reason: Some(flotilla_protocol::StallReason::Infra),
                                proposed_disposition: Some(flotilla_protocol::StallProposedDisposition::Resume),
                                nudge_history: vec![],
                            }),
                            ..Default::default()
                        })
                        .await
                        .expect("stalled status");
                }
                if replicated {
                    backend
                        .replica_writer::<ResourceConvoy>(NodeId::new("remote"), &namespace)
                        .replace(&convoys.list().await.expect("remote convoys"), now)
                        .await
                        .expect("replicate");
                }
            }
            for full in [false, true] {
                let response = ReadProjections::crew_stalls(&backend, full, now).await.expect("stalls");
                let obligations_per_convoy = if unrecognized { 1 } else { roles };
                assert_eq!(response.rows.len(), count * obligations_per_convoy);
                assert_eq!(response.full, full);
                let encoded = serde_json::to_value(&response).expect("JSON");
                assert_eq!(encoded["rows"].as_array().expect("rows").len(), count * obligations_per_convoy);
                for row in response.rows {
                    assert_eq!(row.convoy, "stalled");
                    assert_eq!(row.project_display_name.as_deref(), Some("Project display"));
                    assert_eq!(row.convoy_display_name, "implement #2");
                    assert_eq!(row.age_seconds, Some(if future { 0 } else { 300 }));
                    assert_eq!(row.vessel.is_empty(), unrecognized);
                    assert_eq!(row.role.is_empty(), unrecognized);
                    assert_eq!(row.shared_cause_count, if shared { count * obligations_per_convoy } else { obligations_per_convoy });
                    assert_eq!(
                        row.rung,
                        Some(
                            [
                                flotilla_resources::StallRung::Nudge,
                                flotilla_resources::StallRung::Supervisor,
                                flotilla_resources::StallRung::Bosun,
                                flotilla_resources::StallRung::Governor,
                                flotilla_resources::StallRung::Operator
                            ][rung]
                        )
                    );
                    assert_eq!(row.supervisor.as_deref(), (rung != 4).then_some("governor/control/governor"));
                    assert_eq!(row.supervisor_absence_reason.as_deref(), (rung == 4).then_some(row.evidence.as_str()));
                    assert_eq!(row.proposed_disposition, Some(flotilla_protocol::StallProposedDisposition::Resume));
                }
            }
        });
    }

    // #2498: a stalled crew remains listed after its visible condition is replaced or absent.
    #[tokio::test]
    async fn crew_stalls_retains_crew_without_current_condition() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let convoys = backend.using::<ResourceConvoy>("other-namespace");
        let convoy = convoys
            .create(&InputMeta::builder().name("stalled".into()).build(), &ConvoySpec::builder().workflow_ref("scratch".into()).build())
            .await
            .expect("convoy");
        convoys
            .update_status("stalled", &convoy.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Active,
                crew_work: BTreeMap::from([(
                    "work".into(),
                    BTreeMap::from([
                        ("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Stalled).message("rate limit".into()).build()),
                        ("reviewer".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build()),
                    ]),
                )]),
                ..Default::default()
            })
            .await
            .expect("status");
        let response = ReadProjections::crew_stalls(&backend, false, Utc::now()).await.expect("stalls");
        assert_eq!(response.rows.len(), 1);
        let row = &response.rows[0];
        assert_eq!(row.role, "coder");
        assert_eq!(row.rung, None);
        assert_eq!(row.age_seconds, None);
        assert_eq!(row.evidence, "rate limit");
        assert_eq!(row.supervisor_absence_reason.as_deref(), Some("no current stall condition recorded for this obligation"));
    }

    struct ProjectionFixture {
        temp: tempfile::TempDir,
        backend: ResourceBackend,
        config: Arc<ConfigStore>,
        registry: crate::host_registry::HostRegistry,
        environments: EnvironmentManager,
        host_name: HostName,
        node_id: NodeId,
        clock: Arc<dyn Clock>,
        subscriptions: LeafSubscriptionTable,
        fleet: FleetService,
        event_sink: Arc<dyn EventSink>,
    }

    impl ProjectionFixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("tempdir");
            let backend = ResourceBackend::InMemory(InMemoryBackend::default());
            let config = Arc::new(ConfigStore::with_base(temp.path()));
            let event_sink: Arc<dyn EventSink> = Arc::new(crate::event_sink::RecordingEventSink::default());
            let runner: Arc<dyn crate::providers::CommandRunner> = Arc::new(ProcessCommandRunner);
            let host_name = HostName::new("local");
            let host_id = HostId::new("local-id");
            let node_id = NodeId::new("local-node");
            let environment_id = EnvironmentId::host(host_id.clone());
            let node = NodeInfo::new(node_id.clone(), "local");
            let summary = HostSummary::builder()
                .environment_id(environment_id.clone())
                .host_name(host_name.clone())
                .node(node.clone())
                .system(SystemInfo::default())
                .build();
            let registry = crate::host_registry::HostRegistry::new(node, summary);
            let environments = EnvironmentManager::from_local_state(environment_id, host_id, Arc::clone(&runner), EnvironmentBag::new());
            let refresher = ChangeRequestRefresher::new(
                "fleet".to_string(),
                backend.clone(),
                node_id.to_string(),
                Arc::new(GhChangeRequestObservationSource::new(Arc::clone(&runner))),
                ChangeRequestRefreshCadence::default(),
            );
            let subscriptions = LeafSubscriptionTable::new(backend.clone(), Arc::clone(&event_sink), refresher);
            let fleet =
                FleetService::new(Arc::clone(&event_sink), backend.clone(), AggregatorProjectionState::new(), host_name.clone(), None);
            Self {
                temp,
                backend,
                config,
                registry,
                environments,
                host_name,
                node_id,
                clock: Arc::new(SystemClock),
                subscriptions,
                fleet,
                event_sink,
            }
        }

        fn projections(&self) -> ReadProjections<'_> {
            ReadProjections {
                _event_sink: Arc::clone(&self.event_sink),
                backend: &self.backend,
                config: &self.config,
                host_registry: &self.registry,
                environment_manager: &self.environments,
                host_name: &self.host_name,
                node_id: &self.node_id,
                clock: &self.clock,
                leaf_subscriptions: &self.subscriptions,
                fleet: &self.fleet,
            }
        }

        async fn register_remote_host(&self, node_id: &str, host_name: &str, host_id: &str) {
            let summary = HostSummary::builder()
                .environment_id(EnvironmentId::host(HostId::new(host_id)))
                .host_name(HostName::new(host_name))
                .node(NodeInfo::new(NodeId::new(node_id), host_name))
                .system(SystemInfo::default())
                .build();
            self.registry.publish_peer_summary(summary, &|_| {}).await;
            assert_eq!(self.registry.host_name_for_node(&NodeId::new(node_id)).await, Some(HostName::new(host_name)));
        }
    }

    #[tokio::test]
    async fn project_list_summarizes_resolved_issue_bindings() {
        let fixture = ProjectionFixture::new();
        let excluded = flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "old/repo".into() };
        let active = flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "new/repo".into() };
        let second = flotilla_protocol::IssueSource { service: "https://gitlab.com".into(), scope: "other/repo".into() };
        fixture
            .backend
            .using::<Project>("flotilla")
            .create(
                &InputMeta::builder().name("sample".to_string()).build(),
                &ProjectSpec::builder()
                    .display_name("Sample".to_string())
                    .default_workflow_ref("single-agent".to_string())
                    .issue_source_bindings(vec![
                        flotilla_resources::IssueSourceBindingSpec::builder().source(excluded).exclude(true).build(),
                        flotilla_resources::IssueSourceBindingSpec::builder().source(active.clone()).alias("new".to_string()).build(),
                        flotilla_resources::IssueSourceBindingSpec::builder().source(second.clone()).alias("other".to_string()).build(),
                    ])
                    .build(),
            )
            .await
            .expect("project");

        let response = ReadProjections::list_projects(&fixture.backend, "flotilla", fixture.clock.now()).await.expect("project list");
        assert_eq!(response.projects[0].issue_sources, vec![active, second]);
    }

    #[tokio::test]
    async fn fulfilment_projection_joins_kind_with_host_facts() {
        let fixture = ProjectionFixture::new();
        let kinds = fixture.backend.clone().using::<FulfilmentKind>("flotilla");
        kinds
            .create(
                &InputMeta::builder().name("linux-cleat".to_string()).build(),
                &FulfilmentKindSpec::builder()
                    .host_ref("local-id".to_string())
                    .pool("cleat".to_string())
                    .grants(BTreeSet::from([FulfilmentGrant::Platform("linux".to_string())]))
                    .realisation(FulfilmentRealisation::HostDirect)
                    .build(),
            )
            .await
            .expect("kind");
        let hosts = fixture.backend.clone().using::<ResourceHost>("flotilla");
        let host = hosts
            .create(&InputMeta::builder().name("local-id".to_string()).build(), &HostSpec {
                display_name: "local".to_string(),
                connection: Default::default(),
                ..HostSpec::default()
            })
            .await
            .expect("host");
        hosts
            .update_status("local-id", &host.metadata.resource_version, &ResourceHostStatus {
                heartbeat_at: Some(Utc::now()),
                ready: true,
                fulfilment_facts: BTreeMap::from([("linux-cleat".to_string(), flotilla_resources::FulfilmentFacts {
                    gui_session_logged_in: true,
                    ..Default::default()
                })]),
                ..Default::default()
            })
            .await
            .expect("host facts");

        let response = fixture.projections().fulfilment_list("flotilla").await.expect("fulfilment list");
        assert_eq!(response.kinds.len(), 1);
        assert_eq!(response.kinds[0].name, "linux-cleat");
        assert_eq!(response.kinds[0].gui_session_logged_in, Some(true));
        assert_eq!(response.kinds[0].grants, vec!["platform:linux"]);
    }

    #[tokio::test]
    async fn fleet_health_projection_includes_local_fulfilment() {
        let fixture = ProjectionFixture::new();
        fixture
            .backend
            .clone()
            .using::<FulfilmentKind>("flotilla")
            .create(
                &InputMeta::builder().name("local-kind".to_string()).build(),
                &FulfilmentKindSpec::builder()
                    .host_ref("local-id".to_string())
                    .pool("cleat".to_string())
                    .realisation(FulfilmentRealisation::HostDirect)
                    .build(),
            )
            .await
            .expect("kind");
        let hosts = fixture.backend.clone().using::<ResourceHost>("flotilla");
        let host = hosts
            .create(&InputMeta::builder().name("local-id".to_string()).build(), &HostSpec {
                display_name: "local".to_string(),
                connection: Default::default(),
                ..HostSpec::default()
            })
            .await
            .expect("host");
        hosts
            .update_status("local-id", &host.metadata.resource_version, &ResourceHostStatus {
                heartbeat_at: Some(Utc::now()),
                ready: true,
                daemon_rss_bytes: Some(128 * 1024 * 1024),
                ..Default::default()
            })
            .await
            .expect("host status");
        let host_list = fixture.registry.list_hosts(&HashMap::new()).await;
        let response = fixture
            .projections()
            .fleet_health("flotilla", host_list, Vec::new(), Some("local-id".to_string()), Utc::now())
            .await
            .expect("fleet health");
        let local = response.hosts.iter().find(|host| host.host == HostName::new("local")).expect("local host");
        // Fleet rows preserve the heartbeat RSS byte count.
        assert_eq!(local.daemon_rss_bytes, Some(128 * 1024 * 1024));
        assert_eq!(local.fulfilments.len(), 1);
        assert_eq!(local.fulfilments[0].name, "local-kind");
    }

    #[tokio::test]
    async fn fleet_list_projection_reports_configured_unsynced_remote() {
        let fixture = ProjectionFixture::new();
        std::fs::write(fixture.temp.path().join("hosts.toml"), "[hosts.remote]\nhostname = 'remote.example'\n").expect("host config");
        let response = fixture.projections().fleet_list("flotilla", Vec::new(), Utc::now()).await.expect("fleet list");
        assert!(response.rows.is_empty());
        assert_eq!(response.replicas.len(), 1);
        assert_eq!(response.replicas[0].host, HostName::new("remote"));
        assert!(!response.replicas[0].reachable);
        assert!(response.replicas[0].message.as_deref().is_some_and(|message| message.contains("not synced yet")));
    }

    #[tokio::test]
    async fn fleet_list_marks_remote_rows_unreachable_after_replication_failure() {
        let fixture = ProjectionFixture::new();
        let now = Utc::now();
        let last_sync = now - chrono::Duration::seconds(10);
        fixture.register_remote_host("remote-node", "remote", "remote-id").await;
        let remote_backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let remote_hosts = remote_backend.using::<ResourceHost>("flotilla");
        remote_hosts.create(&InputMeta::builder().name("remote-id".to_string()).build(), &HostSpec::default()).await.expect("host");
        fixture
            .backend
            .replica_writer::<ResourceHost>(NodeId::new("remote-node"), "flotilla")
            .replace(&remote_hosts.list().await.expect("remote hosts"), last_sync)
            .await
            .expect("replicate host");
        fixture.fleet.report_resource_replication_failure(&NodeId::new("remote-node"), "convoys", "connection lost").await;
        let row = FleetListRow::builder()
            .convoy("example")
            .vessel("work")
            .crew("coder")
            .crew_state("active")
            .host(HostName::new("remote"))
            .namespace("flotilla")
            .staleness(FleetStaleness::Fresh { last_sync })
            .build();

        let response = fixture.projections().fleet_list("flotilla", vec![row], now).await.expect("fleet list");
        assert_eq!(response.replicas[0].last_sync, Some(last_sync));
        assert!(!response.replicas[0].reachable);
        assert!(response.replicas[0].message.as_deref().is_some_and(|message| message.contains("convoys: connection lost")));
        assert!(matches!(
            &response.rows[0].staleness,
            FleetStaleness::Unreachable { last_sync: Some(sync), message }
                if *sync == last_sync && message.contains("convoys: connection lost")
        ));
    }

    #[tokio::test]
    async fn fleet_list_marks_remote_rows_unreachable_when_sync_is_stale_without_error() {
        let fixture = ProjectionFixture::new();
        let now = Utc::now();
        let last_sync = now - chrono::Duration::hours(1);
        fixture.register_remote_host("remote-node", "remote", "remote-id").await;
        let remote_backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let remote_hosts = remote_backend.using::<ResourceHost>("flotilla");
        remote_hosts.create(&InputMeta::builder().name("remote-id".to_string()).build(), &HostSpec::default()).await.expect("host");
        fixture
            .backend
            .replica_writer::<ResourceHost>(NodeId::new("remote-node"), "flotilla")
            .replace(&remote_hosts.list().await.expect("remote hosts"), last_sync)
            .await
            .expect("replicate host");
        let row = FleetListRow::builder()
            .convoy("example")
            .vessel("work")
            .crew("coder")
            .crew_state("active")
            .host(HostName::new("remote"))
            .namespace("flotilla")
            .staleness(FleetStaleness::Fresh { last_sync })
            .build();

        let response = fixture.projections().fleet_list("flotilla", vec![row], now).await.expect("fleet list");
        assert!(!response.replicas[0].reachable);
        assert!(matches!(
            &response.rows[0].staleness,
            FleetStaleness::Unreachable { last_sync: Some(sync), message }
                if *sync == last_sync && message.contains("stale")
        ));
    }

    #[tokio::test]
    async fn fleet_views_trust_only_host_self_reports_and_merge_configured_hosts() {
        let fixture = ProjectionFixture::new();
        let now = Utc::now();
        std::fs::write(
            fixture.temp.path().join("hosts.toml"),
            "[hosts.remote]\nhostname = 'remote.example'\n[hosts.missing]\nhostname = 'missing.example'\n",
        )
        .expect("host config");
        fixture.register_remote_host("remote-node", "remote", "remote-id").await;
        fixture.register_remote_host("free-node", "free", "free-id").await;

        let remote_backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let remote_hosts = remote_backend.using::<ResourceHost>("flotilla");
        for (name, generation, heartbeat_at) in
            [("remote-id", "trusted", now - chrono::Duration::minutes(3)), ("local-id", "third-party", now)]
        {
            let created =
                remote_hosts.create(&InputMeta::builder().name(name.to_string()).build(), &HostSpec::default()).await.expect("host");
            remote_hosts
                .update_status(name, &created.metadata.resource_version, &ResourceHostStatus {
                    heartbeat_at: Some(heartbeat_at),
                    daemon_generation: Some(generation.to_string()),
                    ..Default::default()
                })
                .await
                .expect("host status");
        }
        fixture
            .backend
            .replica_writer::<ResourceHost>(NodeId::new("remote-node"), "flotilla")
            .replace(&remote_hosts.list().await.expect("remote hosts"), now - chrono::Duration::minutes(2))
            .await
            .expect("replicate remote hosts");

        let free_backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let free_hosts = free_backend.using::<ResourceHost>("flotilla");
        let created =
            free_hosts.create(&InputMeta::builder().name("free-id".to_string()).build(), &HostSpec::default()).await.expect("free host");
        free_hosts
            .update_status("free-id", &created.metadata.resource_version, &ResourceHostStatus {
                heartbeat_at: Some(now),
                daemon_generation: Some("free-generation".to_string()),
                ..Default::default()
            })
            .await
            .expect("free status");
        fixture
            .backend
            .replica_writer::<ResourceHost>(NodeId::new("free-node"), "flotilla")
            .replace(&free_hosts.list().await.expect("free hosts"), now)
            .await
            .expect("replicate free host");

        let list = fixture.projections().fleet_list("flotilla", Vec::new(), now).await.expect("fleet list");
        assert_eq!(list.replicas.len(), 3);
        let remote = list.replicas.iter().find(|row| row.host == HostName::new("remote")).expect("configured remote");
        assert_eq!(remote.generation.as_deref(), Some("trusted"));
        assert!(!remote.reachable, "old replica must be stale");
        let free = list.replicas.iter().find(|row| row.host == HostName::new("free")).expect("unconfigured replicated host");
        assert_eq!(free.generation.as_deref(), Some("free-generation"));
        assert!(free.reachable);
        let missing = list.replicas.iter().find(|row| row.host == HostName::new("missing")).expect("unsynced configured host");
        assert!(!missing.reachable);
        assert!(missing.last_sync.is_none());

        let host_list = fixture.registry.list_hosts(&HashMap::new()).await;
        let health = fixture
            .projections()
            .fleet_health("flotilla", host_list, Vec::new(), Some("local-id".to_string()), now)
            .await
            .expect("fleet health");
        let remote = health.hosts.iter().find(|row| row.host == HostName::new("remote")).expect("remote health");
        assert_eq!(remote.daemon_generation.as_deref(), Some("trusted"));
        assert_eq!(remote.staleness, FleetHostStaleness::Stale);
        let local = health.hosts.iter().find(|row| row.host == HostName::new("local")).expect("local health");
        assert_ne!(local.daemon_generation.as_deref(), Some("third-party"));

        // A second origin with an older heartbeat but newer sync must win in both views.
        fixture.register_remote_host("mirror-node", "remote", "mirror-id").await;
        let mirror_backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let mirror_hosts = mirror_backend.using::<ResourceHost>("flotilla");
        let created = mirror_hosts
            .create(&InputMeta::builder().name("mirror-id".to_string()).build(), &HostSpec::default())
            .await
            .expect("mirror host");
        mirror_hosts
            .update_status("mirror-id", &created.metadata.resource_version, &ResourceHostStatus {
                heartbeat_at: Some(now - chrono::Duration::minutes(10)),
                daemon_generation: Some("newer-sync".to_string()),
                ..Default::default()
            })
            .await
            .expect("mirror status");
        fixture
            .backend
            .replica_writer::<ResourceHost>(NodeId::new("mirror-node"), "flotilla")
            .replace(&mirror_hosts.list().await.expect("mirror hosts"), now - chrono::Duration::minutes(1))
            .await
            .expect("replicate mirror host");
        let list = fixture.projections().fleet_list("flotilla", Vec::new(), now).await.expect("fleet list with mirror");
        let remote = list.replicas.iter().find(|row| row.host == HostName::new("remote")).expect("remote mirror list");
        assert_eq!(remote.generation.as_deref(), Some("newer-sync"));
        assert!(remote.reachable);
        let host_list = fixture.registry.list_hosts(&HashMap::new()).await;
        let health = fixture
            .projections()
            .fleet_health("flotilla", host_list, Vec::new(), Some("local-id".to_string()), now)
            .await
            .expect("fleet health with mirror");
        let remote = health.hosts.iter().find(|row| row.host == HostName::new("remote")).expect("remote mirror health");
        assert_eq!(remote.daemon_generation.as_deref(), Some("newer-sync"));
    }

    #[tokio::test]
    async fn fleet_rows_skip_unmapped_origins_and_keep_local_snapshot_local() {
        let fixture = ProjectionFixture::new();
        fixture.register_remote_host("remote-node", "remote", "remote-id").await;
        let remote_backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let sessions = remote_backend.using::<ResourceTerminalSession>("flotilla");
        sessions
            .create(
                &InputMeta::builder().name("mapped-session".to_string()).build(),
                &TerminalSessionSpec::builder()
                    .env_ref("remote-env".to_string())
                    .role("coder".to_string())
                    .source(TerminalSessionSource::Tool { command: "true".to_string() })
                    .cwd("/tmp".to_string())
                    .pool("passthrough".to_string())
                    .build(),
            )
            .await
            .expect("remote session");
        let stale_sync = Utc::now() - chrono::Duration::minutes(2);
        fixture
            .backend
            .replica_writer::<ResourceTerminalSession>(NodeId::new("remote-node"), "flotilla")
            .replace(&sessions.list().await.expect("remote sessions"), stale_sync)
            .await
            .expect("replicate remote session");
        fixture
            .backend
            .replica_writer::<ResourceTerminalSession>(NodeId::new("unmapped-node"), "flotilla")
            .replace(&sessions.list().await.expect("unmapped sessions"), stale_sync)
            .await
            .expect("replicate unmapped session");

        let merged_rows = fixture.fleet.rows("flotilla", &fixture.registry).await.expect("fleet rows");
        assert_eq!(merged_rows.len(), 1, "unmapped origin must not create a phantom host row");
        assert_eq!(merged_rows[0].host, HostName::new("remote"));
        assert!(matches!(merged_rows[0].staleness, flotilla_protocol::FleetStaleness::Stale { .. }));
    }

    fn subject_status(now: DateTime<Utc>) -> ChangeRequestStatus {
        ChangeRequestStatus {
            title: Observation::unknown(now),
            author: Observation::unknown(now),
            state: Observation::known(ObservedChangeRequestState::Open, now),
            checks: Observation::known(ObservedChecks::Pass, now),
            review_decision: Observation::known(ObservedReviewDecision::Approved, now),
            review_requested_from_owner: Observation::unknown(now),
            head_sha: Observation::unknown(now),
            mergeable: Observation::known(ObservedMergeability::Mergeable, now),
            review: ChangeRequestReviewObservation { actionable_at_head: Observation::known(false, now) },
        }
    }

    // Behaviour: explain preserves the subject entity's values, while each field
    // ages independently and readiness uses its oldest evidence, including at TTL.
    #[hegel::test]
    fn subject_explanation_preserves_values_and_independent_age(tc: hegel::TestCase) {
        // All request states, unknown values, missing status, and ages either side of TTL.
        let state_index = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
        let age = tc.draw(gs::integers::<i64>().min_value(0).max_value(61));
        let missing = tc.draw(gs::booleans());
        let now = Utc.with_ymd_and_hms(2026, 10, 2, 12, 0, 0).single().expect("time");
        let subject = Subject {
            kind: SubjectKind::ChangeRequest,
            source: IssueSource { service: "github.com".into(), scope: "owner/repo".into() },
            id: "42".into(),
        };
        // Pin the exact TTL boundary in every case as well as drawing surrounding ages.
        for (age_input, age) in (0..5).flat_map(|input| [age, 60].map(|age| (input, age))) {
            let mut status = subject_status(now);
            let states = [
                Some(ObservedChangeRequestState::Open),
                Some(ObservedChangeRequestState::Draft),
                Some(ObservedChangeRequestState::Merged),
                Some(ObservedChangeRequestState::Closed),
                None,
            ];
            status.state.value = states[state_index];
            let observed_at = now - ChronoDuration::seconds(age);
            match age_input {
                0 => status.state.observed_at = observed_at,
                1 => status.checks.observed_at = observed_at,
                2 => status.mergeable.observed_at = observed_at,
                3 => status.review_decision.observed_at = observed_at,
                4 => status.review.actionable_at_head.observed_at = observed_at,
                _ => unreachable!("five inputs"),
            }
            let observation = explain_subject_observation(&subject, (!missing).then_some(&status), false, now, Duration::from_secs(60));
            // The JSON explain wire shape round-trips every generated value and age.
            let json = serde_json::to_value(&observation).expect("encode observation");
            assert_eq!(serde_json::from_value::<ExplainedSubjectObservation>(json).expect("decode observation"), observation);
            assert_eq!(observation.subject, subject);
            assert_eq!(
                observation.state.value.as_deref(),
                if missing { None } else { [Some("open"), Some("draft"), Some("merged"), Some("closed"), None][state_index] }
            );
            assert_eq!(
                observation.state.freshness,
                if missing {
                    EvidenceFreshness::Missing
                } else if age_input == 0 && age >= 60 {
                    EvidenceFreshness::Stale
                } else {
                    EvidenceFreshness::Fresh
                }
            );
            let aged = if missing {
                EvidenceFreshness::Missing
            } else if age < 60 {
                EvidenceFreshness::Fresh
            } else {
                EvidenceFreshness::Stale
            };
            assert_eq!(observation.checks.freshness, if age_input == 1 || missing { aged } else { EvidenceFreshness::Fresh });
            assert_eq!(observation.readiness.freshness, aged);
            assert_eq!(
                observation.readiness.value.as_deref(),
                Some(if missing {
                    "awaiting_review_response"
                } else {
                    ["ready_to_merge", "draft", "merged_not_landed", "closed", "awaiting_review_response"][state_index]
                })
            );
            assert_eq!(observation.checks.value.as_deref(), if missing { None } else { Some("pass") });
            assert_eq!(observation.review.value.as_deref(), if missing { None } else { Some("approved") });
            assert_eq!(observation.review_actionable_at_head.value.as_deref(), if missing { None } else { Some("false") });
        }
    }

    // Behaviour: subjects alone determine explain identities, even across repositories;
    // an unrelated observed request must not appear and an unobserved subject stays visible.
    #[tokio::test]
    async fn convoy_explanation_joins_plural_subjects_to_observations() {
        let now = Utc.with_ymd_and_hms(2026, 10, 2, 12, 0, 0).single().expect("time");
        let mut fixture = ProjectionFixture::new();
        fixture.clock = Arc::new(flotilla_resources::VirtualClock::new(now));
        let subject = |scope: &str| Subject {
            kind: SubjectKind::ChangeRequest,
            source: IssueSource { service: "github.com".into(), scope: scope.into() },
            id: "42".into(),
        };
        let declared = ["owner/one", "owner/two", "owner/missing"].map(|scope| DeclaredSubject {
            subject: subject(scope),
            relationship: Relationship::References,
            issue: None,
            change_request: None,
        });
        fixture
            .backend
            .using::<ResourceConvoy>("flotilla")
            .create(
                &InputMeta::builder().name("subjects".to_string()).build(),
                &ConvoySpec::builder().workflow_ref("review".to_string()).subjects(declared.to_vec()).build(),
            )
            .await
            .expect("convoy");
        let requests = fixture.backend.using::<ChangeRequest>("flotilla");
        for scope in ["owner/one", "owner/two", "owner/unrelated"] {
            let record = requests
                .create(
                    &InputMeta::builder().name(flotilla_resources::change_request_record_name("github.com", scope, 42)).build(),
                    &ChangeRequestSpec::builder()
                        .service("github.com".into())
                        .scope(scope.into())
                        .number(42)
                        .observing_authority("local".into())
                        .build(),
                )
                .await
                .expect("request");
            let mut status = subject_status(now);
            if scope == "owner/two" {
                status.state.value = Some(ObservedChangeRequestState::Merged);
            }
            requests.update_status(&record.metadata.name, &record.metadata.resource_version, &status).await.expect("status");
        }
        // #2471: a newer replica under a different name beats an older local
        // canonical record. Its Unknown checks must not inherit local Pass.
        let remote = ResourceBackend::InMemory(InMemoryBackend::default());
        let remote_requests = remote.using::<ChangeRequest>("flotilla");
        let duplicate = remote_requests
            .create(
                &InputMeta::builder().name("aaa-duplicate".into()).build(),
                &ChangeRequestSpec::builder()
                    .service("github.com".into())
                    .scope("owner/one".into())
                    .number(42)
                    .observing_authority("remote".into())
                    .build(),
            )
            .await
            .expect("duplicate");
        let mut remote_status = subject_status(now + ChronoDuration::seconds(1));
        remote_status.state.value = Some(ObservedChangeRequestState::Draft);
        remote_status.checks.value = None;
        remote_requests.update_status("aaa-duplicate", &duplicate.metadata.resource_version, &remote_status).await.expect("remote status");
        fixture
            .backend
            .replica_writer::<ChangeRequest>(NodeId::new("remote"), "flotilla")
            .replace(&remote_requests.list().await.expect("remote list"), now)
            .await
            .expect("replicate duplicate");
        let convoys = fixture.backend.using::<ResourceConvoy>("flotilla");
        let landed = convoys
            .create(
                &InputMeta::builder().name("previous".to_string()).build(),
                &ConvoySpec::builder().workflow_ref("review".to_string()).subjects(vec![declared[1].clone()]).build(),
            )
            .await
            .expect("linked convoy");
        convoys
            .update_status(&landed.metadata.name, &landed.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Landed,
                ..Default::default()
            })
            .await
            .expect("landed");
        let explanation = fixture.projections().explain_convoy("flotilla", "subjects").await.expect("explain");
        // Subject rows preserve declaration order: one, two, missing. Resource
        // list/map order does not choose these identities or their order.
        assert_eq!(explanation.subject_observations.len(), 3);
        assert_eq!(explanation.subject_observations[0].state.value.as_deref(), Some("draft"));
        assert_eq!(explanation.subject_observations[0].checks.value, None);
        assert_eq!(explanation.subject_observations[1].state.value.as_deref(), Some("merged"));
        assert_eq!(explanation.subject_observations[2].state.value, None);
        assert_eq!(explanation.subject_observations[1].readiness.value.as_deref(), Some("closed"));
        assert!(explanation.subject_observations.iter().all(|row| row.subject.source.scope != "owner/unrelated"));
    }

    #[tokio::test]
    async fn convoy_explanation_projection_reports_terminal_record() {
        let fixture = ProjectionFixture::new();
        let convoys = fixture.backend.clone().using::<ResourceConvoy>("flotilla");
        let created = convoys
            .create(
                &InputMeta::builder().name("finished".to_string()).build(),
                &ConvoySpec::builder().workflow_ref("review".to_string()).build(),
            )
            .await
            .expect("convoy");
        convoys
            .update_status("finished", &created.metadata.resource_version, &ConvoyStatus {
                phase: ConvoyPhase::Failed,
                ..Default::default()
            })
            .await
            .expect("terminal status");
        let response = fixture.projections().explain_convoy("flotilla", "finished").await.expect("explanation");
        assert_eq!(response.convoy, "finished");
        assert_eq!(response.phase, "Failed");
    }

    // Convoy explanation uses typed health evidence, so operators can distinguish
    // provider retry from confirmed loss and see when recovery clears the issue.
    #[tokio::test]
    async fn convoy_explanation_reports_terminal_provider_loss_and_recovery() {
        use flotilla_protocol::ExplainedTerminalCondition;
        use flotilla_resources::{apply_status_patch, TerminalSessionStatus, TerminalSessionStatusPatch};

        let fixture = ProjectionFixture::new();
        fixture
            .backend
            .clone()
            .using::<ResourceConvoy>("flotilla")
            .create(
                &InputMeta::builder().name("health-convoy".to_string()).build(),
                &ConvoySpec::builder().workflow_ref("review".to_string()).build(),
            )
            .await
            .expect("convoy");
        let sessions = fixture.backend.clone().using::<ResourceTerminalSession>("flotilla");
        let session = sessions
            .create(
                &InputMeta::builder()
                    .name("health-session".to_string())
                    .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "health-convoy".to_string())]))
                    .build(),
                &flotilla_resources::TerminalSessionSpec {
                    env_ref: "env".into(),
                    role: "coder".into(),
                    cwd: "/work".into(),
                    pool: "cleat".into(),
                    source: TerminalSessionSource::Tool { command: "sh".into() },
                },
            )
            .await
            .expect("session");
        sessions
            .update_status("health-session", &session.metadata.resource_version, &TerminalSessionStatus {
                phase: flotilla_resources::TerminalSessionPhase::Running,
                ..Default::default()
            })
            .await
            .expect("running");
        apply_status_patch(&sessions, "health-session", &TerminalSessionStatusPatch::MarkReconcileDegraded {
            message: "provider offline".into(),
            consecutive_failures: 100,
            observed_at: Utc::now(),
        })
        .await
        .expect("degradation");
        let explanation = fixture.projections().explain_convoy("flotilla", "health-convoy").await.expect("explanation");
        assert!(
            matches!(&explanation.crew_deliveries[0].terminal_condition, Some(ExplainedTerminalCondition::ProviderUnavailable { message }) if message == "provider offline")
        );
        apply_status_patch(&sessions, "health-session", &TerminalSessionStatusPatch::MarkLost {
            reason: "daemon generation dead".into(),
            lost_at: Utc::now(),
        })
        .await
        .expect("confirmed loss");
        let explanation = fixture.projections().explain_convoy("flotilla", "health-convoy").await.expect("explanation");
        assert!(matches!(&explanation.crew_deliveries[0].terminal_condition, Some(ExplainedTerminalCondition::SessionLost { .. })));
        apply_status_patch(&sessions, "health-session", &TerminalSessionStatusPatch::MarkRevived).await.expect("revive");
        apply_status_patch(&sessions, "health-session", &TerminalSessionStatusPatch::ClearReconcileDegraded).await.expect("recovered");
        let explanation = fixture.projections().explain_convoy("flotilla", "health-convoy").await.expect("explanation");
        assert!(explanation.crew_deliveries[0].terminal_condition.is_none());
    }

    #[test]
    fn completed_claims_without_a_decision_ledger_are_visible_in_explanations() {
        let claimed_at = chrono::Utc.with_ymd_and_hms(2026, 8, 21, 12, 0, 0).single().expect("timestamp");
        let status = ConvoyStatus {
            crew_work: BTreeMap::from([(
                "work".to_string(),
                BTreeMap::from([
                    ("coder".to_string(), CrewWorkState::builder().phase(CrewWorkPhase::Done).finished_at(claimed_at).build()),
                    (
                        "reviewer".to_string(),
                        CrewWorkState::builder()
                            .phase(CrewWorkPhase::Done)
                            .finished_at(claimed_at)
                            .decision_ledger_ref("https://example.test/pull/1#comment-2".to_string())
                            .build(),
                    ),
                ]),
            )]),
            ..Default::default()
        };

        let ledgers = explained_decision_ledgers(Some(&status));
        assert_eq!(ledgers.len(), 2);
        assert!(ledgers.iter().any(|ledger| ledger.role == "coder" && ledger.missing && ledger.comment_url.is_none()));
        assert!(ledgers.iter().any(|ledger| {
            ledger.role == "reviewer" && !ledger.missing && ledger.comment_url.as_deref() == Some("https://example.test/pull/1#comment-2")
        }));
    }

    #[tokio::test]
    async fn credential_alerts_match_the_exact_convoy_and_vessel() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let demand = backend
            .using::<ResourceDemand>("flotilla")
            .create(
                &InputMeta::builder()
                    .name("credential-refresh-conv-implement-review-github-app".to_string())
                    .annotations(BTreeMap::from([
                        ("flotilla.work/credential-refresh-vessel".to_string(), "implement-review".to_string()),
                        ("flotilla.work/credential-refresh-reason".to_string(), "github-app expires in 2 minutes".to_string()),
                    ]))
                    .build(),
                &DemandSpec::for_dispatching_principal(
                    flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "conv"),
                    DemandKind::HumanGate,
                    flotilla_resources::PrincipalRef::implicit_for_namespace("flotilla"),
                ),
            )
            .await
            .expect("create credential attention demand");
        assert_eq!(
            credential_refresh_alert_for_vessel(&demand, "conv", "implement-review"),
            Some("github-app expires in 2 minutes".to_string())
        );
        assert_eq!(credential_refresh_alert_for_vessel(&demand, "conv", "implement"), None);
        assert_eq!(credential_refresh_alert_for_vessel(&demand, "other-convoy", "implement-review"), None);
    }

    #[tokio::test]
    async fn project_and_dispatch_queries_project_in_memory_records() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let projects = backend.clone().definitions::<Project>("flotilla");
        let earlier = Utc::now() - ChronoDuration::minutes(3);
        let observed_at = earlier + ChronoDuration::minutes(2);
        let issue = flotilla_protocol::IssueRef {
            source: flotilla_protocol::IssueSource { service: "https://github.com".into(), scope: "acme/app".into() },
            id: "42".into(),
        };
        projects
            .create(
                &InputMeta::builder().name("app".to_string()).build(),
                &ProjectSpec::builder().display_name("App".to_string()).default_workflow_ref("implement".to_string()).build(),
            )
            .await
            .expect("create project");
        let current = backend.clone().using::<Project>("flotilla").get("app").await.expect("read project");
        backend
            .clone()
            .using::<Project>("flotilla")
            .update_status("app", &current.metadata.resource_version, &ProjectStatus {
                dispatch_queue: vec![DispatchQueueEntry {
                    issue: issue.clone(),
                    title: "Fix query".into(),
                    issue_as_of: earlier,
                    ready_observed_at: earlier,
                    observed_at: earlier,
                    provenance: "test".into(),
                }],
                ..ProjectStatus::default()
            })
            .await
            .expect("publish dispatch status");

        let listed = ReadProjections::list_projects(&backend, "flotilla", Utc::now()).await.expect("project list");
        assert_eq!(listed.projects.len(), 1);
        assert_eq!(listed.projects[0].name, "app");
        assert_eq!(listed.projects[0].display_name, "App");
        let queued = ReadProjections::dispatch_queue(&backend, "flotilla", Some("app"), observed_at).await.expect("dispatch queue");
        assert_eq!(queued.entries.len(), 1);
        assert_eq!(queued.entries[0].issue, issue);
        assert_eq!(queued.entries[0].age_seconds, 120);
        assert!(!queued.entries[0].attention);
        assert!(ReadProjections::dispatch_queue(&backend, "flotilla", Some("other"), observed_at)
            .await
            .expect("filtered queue")
            .entries
            .is_empty());
    }
}
