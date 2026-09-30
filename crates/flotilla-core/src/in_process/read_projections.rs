//! Read-side query projections over resource state and explicit runtime inputs.

use super::*;
use crate::event_sink::EventSink;

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
        local_rows: Vec<FleetListRow>,
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
        let mut host_refs = HashMap::<String, HostName>::new();
        let resource_hosts =
            self.backend.clone().including_replicas::<ResourceHost>(namespace).list().await.map_err(|error| error.to_string())?;
        for resource_host in resource_hosts.items {
            let host = match &resource_host.provenance {
                ResourceProvenance::Local if local_host_id.as_deref() == Some(resource_host.object.metadata.name.as_str()) => {
                    Some(self.host_name.clone())
                }
                ResourceProvenance::Local => None,
                ResourceProvenance::Replica { origin_root, .. } => {
                    // An origin replicates every Host it observes, including this daemon's Host.
                    // Only the Host matching the origin's canonical environment is its self-report.
                    let is_self_report = self
                        .host_registry
                        .environment_id_for_node(origin_root)
                        .await
                        .and_then(|environment_id| environment_id.host_id().map(ToString::to_string))
                        .is_some_and(|host_id| host_id == resource_host.object.metadata.name);
                    if !is_self_report {
                        None
                    } else {
                        self.host_registry.host_name_for_node(origin_root).await.or_else(|| configured_by_node.get(origin_root).cloned())
                    }
                }
            };
            let (Some(host), Some(status)) = (host, resource_host.object.status) else {
                continue;
            };
            host_refs.insert(resource_host.object.metadata.name, host.clone());
            let replace = statuses.get(&host).is_none_or(|current| current.heartbeat_at < status.heartbeat_at);
            if replace {
                statuses.insert(host, status);
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
        let mut rows = self
            .fleet
            .with_health(|replicas| {
                let mut counts = HashMap::<HostName, (usize, HashSet<String>)>::new();
                accumulate_fleet_health_counts(&mut counts, &local_rows);
                let mut surface_by_convoy = HashMap::new();
                for row in local_rows.iter().chain(replicas.values().flat_map(|entry| entry.rows.iter())) {
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
                for entry in replicas.values() {
                    accumulate_fleet_health_counts(&mut counts, &entry.rows);
                }

                let mut rows = Vec::with_capacity(host_rows.len());
                for (host, (is_local, configured, link)) in host_rows {
                    let status = statuses.get(&host);
                    let replica = replicas.get(&host);
                    let heartbeat_at = status.and_then(|status| status.heartbeat_at);
                    let heartbeat_fresh =
                        heartbeat_at.is_some_and(|at| now.signed_duration_since(at).num_seconds() <= HEARTBEAT_READY_TTL_SECS);
                    let replica_fresh = is_local
                        || replica.is_some_and(|replica| {
                            replica.last_error.is_none()
                                && replica
                                    .last_sync
                                    .is_some_and(|at| now.signed_duration_since(at).num_seconds() <= FLEET_REPLICA_FRESH_SECS)
                        });
                    let daemon_generation = status.and_then(|status| status.daemon_generation.clone());
                    let replica_generation =
                        if is_local { daemon_generation.clone() } else { replica.and_then(|replica| replica.generation.clone()) };
                    let staleness = if heartbeat_fresh && replica_fresh {
                        FleetHostStaleness::Current
                    } else if heartbeat_at.is_some() || replica.and_then(|replica| replica.last_sync).is_some() {
                        FleetHostStaleness::Stale
                    } else {
                        FleetHostStaleness::Unknown
                    };
                    let observation_agreement = fleet_observation_agreement(
                        &link,
                        heartbeat_at,
                        heartbeat_fresh,
                        daemon_generation.as_deref(),
                        replica_generation.as_deref(),
                        is_local,
                    );
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
                            .maybe_replica_last_sync(if is_local { Some(now) } else { replica.and_then(|replica| replica.last_sync) })
                            .maybe_replica_generation(replica_generation)
                            .crew_count(crew_count)
                            .convoy_count(convoys.len())
                            .surface_states(surface_states)
                            .maybe_disk_free_bytes(status.and_then(|status| status.disk_free_bytes))
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
            })
            .await;
        rows.sort_by(|left, right| right.is_local.cmp(&left.is_local).then_with(|| left.host.cmp(&right.host)));
        let dispatch_queue = Self::dispatch_queue(self.backend, namespace, None, Utc::now()).await?;
        Ok(FleetHealthResponse { hosts: rows, dispatch_queue })
    }

    pub(super) async fn list_projects(backend: &ResourceBackend, namespace: &str) -> Result<ProjectListResponse, String> {
        let projects = backend.clone().definitions::<Project>(namespace).list().await.map_err(|error| error.to_string())?;
        let repositories = backend.clone().using::<Repository>(namespace).list().await.map_err(|error| error.to_string())?;
        let repositories = repositories
            .items
            .into_iter()
            .map(|repository| (RepositoryKey(repository.metadata.name.clone()), repository))
            .collect::<Vec<_>>();
        let repository_slugs = repository_display_labels(repositories.iter().map(|(key, repository)| (key, &repository.spec)));

        let mut entries = projects
            .into_iter()
            .map(|project| {
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
                ProjectListEntry::builder()
                    .namespace(project.metadata.namespace.clone())
                    .name(project.metadata.name.clone())
                    .display_name(project.spec.display_name)
                    .address(ViewAddress::Project { namespace: project.metadata.namespace, name: project.metadata.name })
                    .repositories(repositories)
                    .maybe_issue_source(project.spec.issue_source_bindings.first().map(|binding| binding.source.clone()))
                    .default_workflow_ref(project.spec.default_workflow_ref)
                    .conflicts(conflicts)
                    .build()
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| (&left.namespace, &left.name).cmp(&(&right.namespace, &right.name)));
        Ok(ProjectListResponse { projects: entries })
    }

    pub(super) async fn get_host_status(
        &self,
        environment_id: &EnvironmentId,
        counts: &HashMap<EnvironmentId, HostCounts>,
        local_summary: &HostSummary,
        namespace: &str,
    ) -> Result<HostStatusResponse, String> {
        let mut response = self.host_registry.get_host_status(environment_id, counts).await?;
        if let Some(host_id) = environment_id.host_id() {
            response.blob_sync = match self.backend.including_replicas::<ResourceHost>(namespace).get(host_id.as_str()).await {
                Ok(host) => host.object.status.and_then(|status| status.blob_sync),
                Err(ResourceError::NotFound { .. }) => None,
                Err(error) => return Err(error.to_string()),
            };
        }
        if environment_id == &local_summary.environment_id {
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
        if environment_id == &local_summary.environment_id {
            response.visible_environments = self.environment_manager.visible_environments().await;
        }
        Ok(response)
    }

    pub(super) async fn fleet_list(&self, mut rows: Vec<FleetListRow>, now: DateTime<Utc>) -> Result<FleetListResponse, String> {
        let mut replicas = Vec::new();
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
        self.fleet
            .with_health(|cache| {
                for (label, remote) in configured_hosts {
                    let host = HostName::new(remote.expected_host_name);
                    let replication_failures = replication_failures_by_host.remove(&host).unwrap_or_default();
                    let replication_error = format_resource_replication_failures(&replication_failures);
                    match cache.get(&host) {
                        Some(entry) => {
                            let staleness = replica_staleness(entry, now);
                            rows.extend(entry.rows.iter().cloned().map(|mut row| {
                                row.staleness = staleness.clone();
                                row
                            }));
                            replicas.push(FleetReplicaStatus {
                                host,
                                reachable: entry.last_error.is_none() && replication_error.is_none(),
                                last_sync: entry.last_sync,
                                generation: entry.generation.clone(),
                                skipped_records: entry.skipped_records,
                                first_parse_error: entry.first_parse_error.clone(),
                                message: join_replica_errors(entry.last_error.as_deref(), replication_error.as_deref()),
                            });
                        }
                        None => {
                            let unsynced = format!("replica source '{label}' has not synced yet");
                            replicas.push(FleetReplicaStatus {
                                host,
                                reachable: false,
                                last_sync: None,
                                generation: None,
                                skipped_records: 0,
                                first_parse_error: None,
                                message: join_replica_errors(Some(&unsynced), replication_error.as_deref()),
                            });
                        }
                    }
                }
            })
            .await;
        for (host, failures) in replication_failures_by_host {
            replicas.push(FleetReplicaStatus {
                host,
                reachable: false,
                last_sync: None,
                generation: None,
                skipped_records: 0,
                first_parse_error: None,
                message: format_resource_replication_failures(&failures),
            });
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
        Ok(FleetListResponse { rows, replicas })
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
        let mut selected_change_requests = BTreeMap::new();
        for source in &change_request_sources {
            let name = source.object.metadata.name.clone();
            let replace = selected_change_requests.get(&name).is_none_or(
                |existing: &&flotilla_resources::ReadResourceObject<flotilla_resources::ChangeRequest>| {
                    !matches!(existing.provenance, ResourceProvenance::Local) && matches!(source.provenance, ResourceProvenance::Local)
                },
            );
            if replace {
                selected_change_requests.insert(name, source);
            }
        }
        let change_request_objects =
            selected_change_requests.iter().map(|(name, source)| (name.clone(), source.object.clone())).collect::<BTreeMap<_, _>>();
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
                    observation_errors.insert(subject.record_name(), error);
                }
            }
        }
        let change_requests = expected_change_requests
            .iter()
            .map(|record_name| {
                let selected = selected_change_requests.get(record_name).copied();
                let observed_at = selected.and_then(|source| source.object.status.as_ref()).map(|status| status.state.observed_at);
                ExplainedChangeRequest {
                    name: record_name.clone(),
                    bound: bound_name.as_ref() == Some(record_name),
                    observed: selected.is_some(),
                    provenance: selected.map(|source| explained_provenance(&source.provenance, self.node_id)),
                    fields: selected.and_then(|source| serde_json::to_value(&source.object).ok()),
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

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use flotilla_resources::{CrewWorkState, DispatchQueueEntry, ProjectStatus};

    use super::*;

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

        let listed = ReadProjections::list_projects(&backend, "flotilla").await.expect("project list");
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
