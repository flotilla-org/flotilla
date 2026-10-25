//! Attach target indexing, role resolution, and plan construction.

use flotilla_discovery::environment_manager::{EnvironmentManager, ManagedEnvironmentKind};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::Path,
    str::FromStr,
    sync::Arc,
};

use flotilla_charter::project_declaration::BOOTSTRAP_REPOSITORY_ANNOTATION;
use flotilla_config::ConfigStore;
use flotilla_environment::hop_chain::remote::{SshConnection, SshRemoteHopResolver};
use flotilla_projections::ports::ProvisioningKind;

use flotilla_protocol::{
    arg::Arg,
    commands::AttachMode,
    qualified_path::HostId,
    result_set::{CheckoutRow, Rows},
    AttachBinding, CanonicalHostId, ConvoyPhase, EnvironmentId, FleetListRow, FleetStaleness, HostName, LifecycleReading,
    ResolvedAttachAction, ResolvedAttachPlan,
};
use flotilla_resources::{
    terminal_session_attach_target_with_stale_status, Convoy as ResourceConvoy, ConvoyPhase as ResourceConvoyPhase,
    Environment as ResourceEnvironment, Host as ResourceHost, Project, ReadResourceObject, ResourceProvenance,
    TerminalSession as ResourceTerminalSession, CONVOY_LABEL, ROLE_LABEL, VESSEL_LABEL, VESSEL_REF_LABEL,
};
use flotilla_store::{ResourceBackend, StoreQuery};
use sha2::{Digest, Sha256};

use super::{discover_repo_for_environment, LiveConvoyRecord};
use flotilla_discovery::{registry::ProviderRegistry, DiscoveryRuntime};
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_projections::aggregator_projection::AggregatorProjectionState;
use flotilla_provider_runtime::host_environment::HostEnvironment;
use flotilla_resources::convoy_address::{convoy_address, RoleAddress};
use flotilla_terminal::TerminalSessionLiveness;

// A held environment can only attach to its realised backing; unheld environments use the legacy handle.
fn environment_attach_backing(
    reads: &dyn flotilla_resources::lifecycle_reads::LifecycleReads,
    name: &str,
) -> Result<Option<String>, String> {
    match reads.try_reading(ProvisioningKind::Environment, name, None)? {
        Some(LifecycleReading::Ready { identity }) => Ok(Some(identity)),
        Some(reading) => Err(format!("environment {name} is {}: {}", reading.label(), reading.reason().unwrap_or_default())),
        None => Ok(None),
    }
}

type HostSnapshot = Result<Vec<ReadResourceObject<ResourceHost>>, String>;

/// One lazy Host snapshot per attach request (including failed loads). Namespace
/// keys keep resource namespaces isolated; dropping the request releases all data.
#[derive(Default)]
struct AttachReadContext {
    hosts: tokio::sync::Mutex<HashMap<String, HostSnapshot>>,
}

impl AttachReadContext {
    async fn canonical_host(
        &self,
        backend: &ResourceBackend,
        namespace: &str,
        host_ref: &str,
    ) -> Result<Option<flotilla_protocol::PlacementTargetHost>, String> {
        self.canonical_host_with_load(namespace, host_ref, async {
            backend.including_replicas::<ResourceHost>(namespace).list().await.map(|hosts| hosts.items).map_err(|error| error.to_string())
        })
        .await
    }

    async fn canonical_host_with_load(
        &self,
        namespace: &str,
        host_ref: &str,
        load: impl std::future::Future<Output = HostSnapshot>,
    ) -> Result<Option<flotilla_protocol::PlacementTargetHost>, String> {
        let mut snapshots = self.hosts.lock().await;
        if !snapshots.contains_key(namespace) {
            snapshots.insert(namespace.to_string(), load.await);
        }
        match snapshots.get(namespace).expect("Host snapshot initialized") {
            Ok(hosts) => flotilla_resources::canonical_placement_host_ref_from_sources(hosts, host_ref),
            Err(error) => Err(error.clone()),
        }
    }
}

pub(super) struct AttachResolver<'a> {
    pub(super) resource_backend: &'a ResourceBackend,
    pub(super) observed_resource_backend: &'a ResourceBackend,
    pub(super) aggregator_projection_state: &'a AggregatorProjectionState,
    pub(super) config: &'a ConfigStore,
    pub(super) host_registry: &'a flotilla_projections::host_registry::HostRegistry,
    pub(super) environment_manager: &'a EnvironmentManager,
    pub(super) discovery: &'a DiscoveryRuntime,
    pub(super) local_environment_id: &'a EnvironmentId,
    pub(super) host_name: &'a HostName,
    pub(super) namespace: &'a std::sync::RwLock<String>,
    /// The engine's provisioning readings behind `[primitives]` (#3629 layer 4).
    pub(super) lifecycle_reads: &'a dyn flotilla_projections::ports::LifecycleReads,
    /// Sessions on agentless hosts attach through their vessel's agent (#3792).
    pub(super) agentless_vessels: Option<Arc<dyn super::AgentlessVesselAccess>>,
}

impl<'a> AttachResolver<'a> {
    async fn provisioning_namespace(&self) -> String {
        self.namespace.read().expect("provisioning namespace lock poisoned").clone()
    }

    async fn aggregator_projection_state(&self) -> AggregatorProjectionState {
        self.aggregator_projection_state.clone()
    }

    fn local_host_id(&self) -> Option<HostId> {
        self.environment_manager.host_id_for_environment(self.local_environment_id)
    }

    fn environment_registry_for_environment(&self, env_id: &EnvironmentId) -> Option<Arc<ProviderRegistry>> {
        self.environment_manager.environment_registry(env_id)
    }

    pub(super) async fn resolve_attach(
        &self,
        reference: &str,
        host: Option<&HostName>,
        transient: bool,
        mode: AttachMode,
        project_context: Option<&str>,
    ) -> Result<ResolvedAttach, String> {
        if let Some(id) = reference.strip_prefix(flotilla_protocol::commands::ENVIRONMENT_SHELL_REFERENCE_PREFIX) {
            return self.environment_shell(id, reference, host, mode).await;
        }
        self.resolve_attach_with_context(reference, host, transient, mode, project_context).await
    }

    async fn environment_shell(
        &self,
        id: &str,
        reference: &str,
        host: Option<&HostName>,
        mode: AttachMode,
    ) -> Result<ResolvedAttach, String> {
        let namespace = self.provisioning_namespace().await;
        let environment = self.resource_backend.using::<ResourceEnvironment>(&namespace).get(id).await.map_err(|e| e.to_string())?;
        let spec = environment.spec.docker.as_ref().ok_or("environment shell requires a contained environment")?;
        let owner = self.target_host_with_reads(&AttachReadContext::default(), &namespace, &spec.host_ref).await?;
        if host.is_some_and(|requested| requested != &owner) {
            return Err(format!("environment {id} is not on the requested host"));
        }
        let binding = AttachBinding::builder().host(owner.clone()).namespace(namespace).session(reference.to_string()).build();
        if owner != *self.host_name {
            return Ok(ResolvedAttach {
                plan: self.recursive_attach_plan_for_remote(&owner, reference, mode).await?,
                binding: Some(binding),
            });
        }
        let handle = self
            .environment_manager
            .managed_environments()
            .into_iter()
            .find_map(|(name, state)| {
                if name.as_str() != id {
                    return None;
                }
                match state {
                    ManagedEnvironmentKind::Provisioned(state) => Some(state.handle),
                    _ => None,
                }
            })
            .ok_or("environment has no live handle")?;
        let cwd = ExecutionEnvironmentPath::new(handle.capabilities().workspace_root);
        let agent = handle.agent();
        if let Some(agent) = agent {
            if agent.ready().await.is_ok() {
                let registry = self.registry_for_resource_environment(&AttachReadContext::default(), &environment, cwd.as_path()).await?;
                let pool = registry.terminal_pools.get("cleat").map(|(_, pool)| pool.clone()).ok_or("environment has no cleat pool")?;
                let session = format!("flotilla-shell-{}", uuid::Uuid::new_v4());
                pool.ensure_session(&session, "exec \"${SHELL:-/bin/sh}\" -l", &cwd, &Vec::new(), &[]).await?;
                let endpoint = pool.cleat_endpoint(&session).await?.ok_or("shell has no cleat endpoint")?;
                let guest = Path::new(&endpoint.runtime_root).join(&endpoint.daemon).join("socket");
                let socket = agent.forward_socket(&guest).await?;
                return Ok(ResolvedAttach { plan: cleat_socket_attach_plan(&socket, &endpoint.session, mode), binding: Some(binding) });
            }
        }
        let plan = handle.fallback_shell_plan(cwd.as_path()).ok_or("agent unavailable and environment has no fallback shell")?;
        Ok(ResolvedAttach { plan, binding: Some(binding) })
    }

    pub(super) async fn resolve_transient(&self, reference: &str, host: Option<&HostName>) -> Result<ResolvedAttach, String> {
        if reference.trim().is_empty() {
            return Err("attach reference is required".to_string());
        }
        self.resolve_attach(reference, host, true, AttachMode::Default, None).await
    }

    pub(super) async fn resolvable_references(&self, references: &[String]) -> Result<HashSet<String>, String> {
        if references.is_empty() {
            return Ok(HashSet::new());
        }
        let index = self.attach_candidate_index(references).await?;
        let mut resolved = HashSet::new();
        for reference in references {
            if index.resolve(self, reference, None, false, AttachMode::Default).await.is_ok() {
                resolved.insert(reference.clone());
            }
        }
        Ok(resolved)
    }

    pub(super) async fn resolvable_targets(&self, targets: &[(String, HostName)]) -> Result<Vec<bool>, String> {
        let index = self.attach_candidate_index(&targets.iter().map(|(reference, _)| reference.clone()).collect::<Vec<_>>()).await?;
        let mut resolved = Vec::with_capacity(targets.len());
        for (reference, host) in targets {
            resolved.push(index.resolve(self, reference, Some(host), false, AttachMode::Default).await.is_ok());
        }
        Ok(resolved)
    }

    pub(super) async fn attach_project_context(
        &self,
        selector: Option<&flotilla_protocol::RepoSelector>,
    ) -> Result<Option<String>, String> {
        let Some(selector) = selector else {
            return Ok(None);
        };
        let namespace = self.provisioning_namespace().await;
        let repository_key = flotilla_admission::repository_addressing::resolve_repository_with_lifecycle_reads(
            self.resource_backend,
            self.observed_resource_backend,
            &namespace,
            self.environment_manager.local_host_id().as_str(),
            selector,
            self.lifecycle_reads,
        )
        .await?;
        let Some(repository_key) = repository_key else {
            // Cwd is optional context for global session identifiers.
            return if matches!(selector, flotilla_protocol::RepoSelector::Path(_)) {
                Ok(None)
            } else {
                Err(format!(
                    "no Repository matches '{selector}'; adopt a checkout with `flotilla repo add <path>` or declare a Project member"
                ))
            };
        };
        let projects = self
            .resource_backend
            .definitions::<Project>(&namespace)
            .query_declared(&StoreQuery {
                fields: BTreeMap::from([("spec.repositories[].repo".into(), repository_key.to_string())]),
                include_replicas: true,
                ..Default::default()
            })
            .await
            .map_err(|error| error.to_string())?;
        let mut matches = projects;
        let repository_key = repository_key.to_string();
        let mut declaration_matches = matches
            .iter()
            .filter(|project| project.metadata.annotations.get(BOOTSTRAP_REPOSITORY_ANNOTATION) == Some(&repository_key))
            .map(|project| project.metadata.name.clone())
            .collect::<Vec<_>>();
        declaration_matches.sort();
        declaration_matches.dedup();
        if let [project] = declaration_matches.as_slice() {
            return Ok(Some(project.clone()));
        }
        matches.sort_by(|left, right| left.metadata.name.cmp(&right.metadata.name));
        matches.dedup_by(|left, right| left.metadata.name == right.metadata.name);
        Ok(matches.as_slice().first().filter(|_| matches.len() == 1).map(|project| project.metadata.name.clone()))
    }

    async fn resolve_live_convoy_record(
        &self,
        reference: &str,
        project_context: Option<&str>,
        observations: &AttachCandidateIndex,
    ) -> Result<Option<LiveConvoyRecord>, String> {
        let explicit = reference.contains('@');
        let requested = if explicit {
            Some(RoleAddress::from_str(reference)?)
        } else {
            project_context.map(|project| RoleAddress { project: project.to_string(), role: reference.to_string() })
        };
        let sources = &observations.convoys;
        let has_role = sources.iter().any(|source| source.object.spec.role == reference);
        if !explicit && requested.is_none() && !has_role {
            return Ok(None);
        }

        let role = requested.as_ref().map_or(reference, |address| address.role.as_str());
        let matching_convoy_exists = sources.iter().any(|source| {
            source.object.spec.role == role
                && requested.as_ref().is_none_or(|address| source.object.spec.project_ref.as_deref() == Some(&address.project))
        });
        let durable_sessions = &observations.sessions;
        let mut sessions_by_convoy = HashMap::<String, Vec<_>>::new();
        for source in durable_sessions {
            if source.object.status.as_ref().and_then(|status| status.session_id.as_ref()).is_some() {
                if let Some(convoy) = source.object.metadata.labels.get(CONVOY_LABEL).cloned() {
                    sessions_by_convoy.entry(convoy).or_default().push((&source.object, &source.provenance));
                }
            }
        }
        let mut candidates = sources
            .iter()
            .filter(|source| {
                source.object.spec.role == role
                    && sessions_by_convoy.contains_key(&source.object.metadata.name)
                    && requested.as_ref().is_none_or(|address| source.object.spec.project_ref.as_deref() == Some(&address.project))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            (&left.object.spec.project_ref, std::cmp::Reverse(left.object.spec.generation), &left.object.metadata.name).cmp(&(
                &right.object.spec.project_ref,
                std::cmp::Reverse(right.object.spec.generation),
                &right.object.metadata.name,
            ))
        });
        let mut connectable = Vec::new();
        for source in candidates {
            let sessions = sessions_by_convoy.get(&source.object.metadata.name).expect("candidate convoy has a session");
            let mut reachable = false;
            for (session, provenance) in sessions {
                reachable = match provenance {
                    ResourceProvenance::Local => self
                        .attach_plan_for_session(&observations.reads, &session.metadata.name, session, AttachMode::Default)
                        .await
                        .is_ok(),
                    ResourceProvenance::Replica { origin_root, .. } => {
                        self.host_registry.live_routed_host_name(origin_root).await.is_some()
                    }
                };
                if reachable {
                    break;
                }
            }
            if reachable {
                connectable.push(source);
            }
        }
        let candidates = connectable;
        let unique_projects = candidates.iter().map(|source| &source.object.spec.project_ref).collect::<BTreeSet<_>>();
        match candidates.as_slice() {
            [] if explicit && !matching_convoy_exists => Err(format!("no live convoy matches `{reference}`")),
            [] => Ok(None),
            [source, ..]
                if unique_projects.len() == 1
                    && candidates.get(1).is_none_or(|next| source.object.spec.generation > next.object.spec.generation) =>
            {
                let project = source
                    .object
                    .spec
                    .project_ref
                    .clone()
                    .ok_or_else(|| format!("live convoy {} has no project identity", source.object.metadata.name))?;
                let owner_host = match &source.provenance {
                    ResourceProvenance::Local => self.host_name.clone(),
                    ResourceProvenance::Replica { origin_root, .. } => self
                        .host_registry
                        .live_routed_host_name(origin_root)
                        .await
                        .ok_or_else(|| format!("owner host for {role}@{project} is unreachable"))?,
                };
                Ok(Some(LiveConvoyRecord {
                    address: RoleAddress { project, role: role.to_string() },
                    record_name: source.object.metadata.name.clone(),
                    owner_host,
                }))
            }
            candidates => {
                let addresses = candidates
                    .iter()
                    .filter_map(|source| {
                        source
                            .object
                            .spec
                            .project_ref
                            .as_ref()
                            .map(|project| RoleAddress { project: project.clone(), role: source.object.spec.role.clone() })
                    })
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .map(|address| address.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(format!("{role} is ambiguous: {addresses}"))
            }
        }
    }

    async fn resolve_attach_with_context(
        &self,
        reference: &str,
        host: Option<&HostName>,
        transient: bool,
        mode: AttachMode,
        project_context: Option<&str>,
    ) -> Result<ResolvedAttach, String> {
        // Preserve validation precedence without paying to build the candidate index.
        if reference.trim().is_empty() {
            return Err("attach reference is required".to_string());
        }
        let index = self.attach_candidate_index(&[reference.to_string()]).await?;
        if let Some(record) = self.resolve_live_convoy_record(reference, project_context, &index).await? {
            if let Some(requested) = host {
                if requested != &record.owner_host {
                    return Err(format!("no attach target matching '{reference}' on host '{requested}'"));
                }
            }
            if record.owner_host != *self.host_name {
                let plan = self.recursive_attach_plan_for_remote(&record.owner_host, &record.address.to_string(), mode).await?;
                let binding = AttachBinding::builder()
                    .host(record.owner_host)
                    .namespace(self.provisioning_namespace().await)
                    .maybe_convoy_phase(
                        index
                            .convoys
                            .iter()
                            .find(|source| source.object.metadata.name == record.record_name)
                            .and_then(|source| source.object.status.as_ref())
                            .map(|status| attach_convoy_phase(status.phase)),
                    )
                    .convoy(record.record_name)
                    .role(record.address.role)
                    .build();
                return Ok(ResolvedAttach { plan, binding: Some(binding) });
            }
            return index.resolve(self, &record.record_name, host, transient, mode).await.map_err(AttachIndexError::into_message);
        }
        match index.resolve(self, reference, host, transient, mode).await {
            Err(AttachIndexError::NoMatch(_)) if reference.starts_with("terminal-") => {
                Err(format!("session {reference} no longer exists on {}", host.unwrap_or(self.host_name)))
            }
            Err(AttachIndexError::NoMatch(error)) => {
                let convoys = &index.convoys;
                let requested_role = reference.contains('@').then(|| RoleAddress::from_str(reference)).transpose()?;
                if convoys.iter().any(|source| {
                    source.object.metadata.name == reference
                        || (source.object.spec.role == reference)
                        || requested_role.as_ref().is_some_and(|address| {
                            source.object.spec.role == address.role && source.object.spec.project_ref.as_deref() == Some(&address.project)
                        })
                }) {
                    Err(format!(
                        "no session available for {reference} on {}; the crew has not started yet or its session has ended",
                        host.unwrap_or(self.host_name)
                    ))
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error.into_message()),
            Ok(resolved) => Ok(resolved),
        }
    }

    async fn attach_candidate_index(&self, references: &[String]) -> Result<AttachCandidateIndex, String> {
        let namespace = self.provisioning_namespace().await;
        let convoy_sources = self
            .resource_backend
            .query_declared::<ResourceConvoy>(&namespace, &convoy_reference_query(references))
            .await
            .map_err(|error| error.to_string())?
            .items;
        let mut query = session_reference_query(references);
        if let Some(alternatives) = query.any_of.as_mut() {
            alternatives.extend(convoy_sources.iter().map(|source| {
                StoreQuery::labels(&BTreeMap::from([(CONVOY_LABEL.into(), source.object.metadata.name.clone())]), false, true)
            }));
        }
        let durable_sessions = self
            .resource_backend
            .query_declared::<ResourceTerminalSession>(&namespace, &query)
            .await
            .map_err(|error| error.to_string())?
            .items;
        let observed_sessions = self
            .observed_resource_backend
            .query_declared::<ResourceTerminalSession>(&namespace, &query)
            .await
            .map_err(|error| error.to_string())?
            .items;
        let convoys = convoy_sources.clone();
        let sessions = durable_sessions.iter().chain(&observed_sessions).cloned().collect();
        let convoy_addresses = convoy_sources
            .into_iter()
            .map(|source| {
                let address = convoy_address(&source.object.spec.role, source.object.spec.project_ref.as_deref());
                ((source.object.metadata.namespace, source.object.metadata.name, replica_origin(&source.provenance).cloned()), address)
            })
            .collect::<HashMap<_, _>>();
        let mut sessions_by_name = HashMap::new();
        let mut replicated_sessions = Vec::new();
        for session in durable_sessions {
            match session.provenance {
                ResourceProvenance::Local => {
                    sessions_by_name.insert(session.object.metadata.name.clone(), session.object);
                }
                ResourceProvenance::Replica { .. } => replicated_sessions.push((session, AttachSessionSource::Durable)),
            }
        }
        let mut replica_keys = replicated_sessions
            .iter()
            .map(|(session, _)| {
                (
                    session.object.metadata.namespace.clone(),
                    session.object.metadata.name.clone(),
                    replica_origin(&session.provenance).cloned(),
                )
            })
            .collect::<HashSet<_>>();
        for session in observed_sessions {
            match session.provenance {
                ResourceProvenance::Local => {
                    sessions_by_name.entry(session.object.metadata.name.clone()).or_insert(session.object);
                }
                ResourceProvenance::Replica { .. } => {
                    if replica_keys.insert((
                        session.object.metadata.namespace.clone(),
                        session.object.metadata.name.clone(),
                        replica_origin(&session.provenance).cloned(),
                    )) {
                        replicated_sessions.push((session, AttachSessionSource::Observed));
                    }
                }
            }
        }
        let mut candidates = Vec::new();
        for session in sessions_by_name.into_values() {
            let convoy_address = session_convoy_address(&session, None, &convoy_addresses);
            candidates.push(AttachCandidate {
                label: attach_reference_label(&session.metadata.name, &session.metadata.labels, convoy_address.map(String::as_str)),
                references: attach_reference_keys(&session.metadata.name, &session.metadata.labels, convoy_address.map(String::as_str)),
                host: self.host_name.clone(),
                target: AttachTarget::Local(Box::new(session)),
            });
        }
        for (replicated, source) in replicated_sessions {
            let session = replicated.object;
            let ResourceProvenance::Replica { origin_root, .. } = replicated.provenance else {
                unreachable!("local durable sessions were partitioned above");
            };
            let Some(host) = self.host_registry.live_routed_host_name(&origin_root).await else {
                continue;
            };
            let convoy_address = session_convoy_address(&session, Some(&origin_root), &convoy_addresses);
            let convoy = session.metadata.labels.get(CONVOY_LABEL).cloned().unwrap_or_else(|| "-".to_string());
            let role = session.metadata.labels.get(ROLE_LABEL).cloned().unwrap_or_else(|| session.spec.role.clone());
            let crew = session.metadata.labels.get(VESSEL_LABEL).map_or_else(|| role.clone(), |vessel| format!("{vessel}/{role}"));
            let independent = !session.metadata.labels.contains_key(CONVOY_LABEL);
            if independent
                && matches!(source, AttachSessionSource::Observed)
                && session.status.as_ref().is_none_or(|status| status.phase != flotilla_resources::TerminalSessionPhase::Running)
            {
                continue;
            }
            let row = FleetListRow::builder()
                .convoy(convoy_address.cloned().unwrap_or_else(|| convoy.clone()).replace("@", " @ "))
                .maybe_convoy_ref((!independent).then_some(convoy))
                .vessel(if independent { "-".to_string() } else { session.spec.env_ref.clone() })
                .crew(if independent { "-".to_string() } else { crew })
                .crew_state(flotilla_projections::fleet::session_status_label(session.status.as_ref().map(|status| status.phase)))
                .host(host.clone())
                .namespace(session.metadata.namespace.clone())
                .session(session.metadata.name.clone())
                .staleness(FleetStaleness::Local)
                .build();
            let mut references =
                attach_reference_keys(&session.metadata.name, &session.metadata.labels, convoy_address.map(String::as_str));
            if !independent {
                references.extend(fleet_row_attach_reference_keys(&row));
            }
            references.sort();
            references.dedup();
            candidates.push(AttachCandidate {
                label: if independent { format!("{} ({host})", session.metadata.name) } else { fleet_row_attach_reference_label(&row) },
                references,
                host,
                target: AttachTarget::Replica { row: Box::new(row) },
            });
        }

        let checkout_set = self
            .aggregator_projection_state()
            .await
            .result_set_for(&flotilla_protocol::QueryId::Checkouts { scope: None })
            .await
            .expect("checkout query is always materialized");
        if let Rows::Checkouts { rows, .. } = checkout_set.rows {
            candidates.extend(rows.into_iter().filter(|row| row.for_convoy.is_none()).map(|row| AttachCandidate {
                label: format!("{} ({})", row.path, row.host),
                references: vec![row.path.clone()],
                host: row.host.clone(),
                target: AttachTarget::Checkout(Box::new(row)),
            }));
        }

        Ok(AttachCandidateIndex::new(candidates, convoys, sessions))
    }

    async fn local_checkout_terminal_plan(&self, checkout: &CheckoutRow, seat: AttachMode) -> Result<ResolvedAttachPlan, String> {
        let cwd = ExecutionEnvironmentPath::new(&checkout.path);
        let discovery = discover_repo_for_environment(
            self.environment_manager,
            self.discovery,
            self.config,
            self.resource_backend,
            &self.provisioning_namespace().await,
            self.local_environment_id,
            self.local_environment_id,
            cwd.as_path(),
            None,
        )
        .await
        .map_err(|error| format!("checkout {} provider discovery failed: {error}", checkout.path))?;
        let pool = discovery
            .registry
            .terminal_pools
            .preferred()
            .cloned()
            .ok_or_else(|| format!("no terminal pool available for checkout {}", checkout.path))?;
        let session_name = transient_checkout_session_name(checkout);
        let command = "${SHELL:-/bin/sh}";
        pool.preflight_attach(seat).await?;
        pool.ensure_session(&session_name, command, &cwd, &Vec::new(), &[]).await?;
        let args = pool.attach_args_for_mode(&session_name, command, &cwd, &Vec::new(), seat)?;
        Ok(ResolvedAttachPlan(vec![ResolvedAttachAction::Command(args)]))
    }

    /// Resolve the attach plan for a locally-known session, returning it
    /// with the host that actually owns the session (the binding host).
    async fn attach_plan_for_session(
        &self,
        reads: &AttachReadContext,
        reference: &str,
        session: &flotilla_resources::ResourceObject<ResourceTerminalSession>,
        seat: AttachMode,
    ) -> Result<(ResolvedAttachPlan, HostName), String> {
        let namespace = self.provisioning_namespace().await;
        let environments = self.resource_backend.clone().using::<ResourceEnvironment>(&namespace);
        let environment = environments
            .get(&session.spec.env_ref)
            .await
            .map_err(|err| format!("environment {} lookup failed: {err}", session.spec.env_ref))?;
        if let Some(handle) = self.checked_provisioned_handle(&environment)? {
            // Host-direct resources without vessel capabilities retain their
            // existing pool/host routing. Every offered vessel capability is
            // selected from the handle, independently of its provider or spec kind.
            if handle.agent().is_some() || handle.terminal_route().is_some() || environment.spec.host_direct.is_none() {
                let plan = self.provisioned_attach_plan_for_session(reads, session, &environment, handle, seat).await?;
                return Ok((plan, self.host_name.clone()));
            }
        }
        let host_ref = environment
            .spec
            .host_direct
            .as_ref()
            .map(|spec| spec.host_ref.as_str())
            .or_else(|| environment.spec.docker.as_ref().map(|spec| spec.host_ref.as_str()))
            .ok_or_else(|| format!("environment {} has no host binding", session.spec.env_ref))?;
        if let Some((destination, host_environment)) = self
            .environment_manager
            .managed_environments()
            .into_iter()
            .find(|(id, _)| id.as_str() == session.spec.env_ref)
            .and_then(|(_, state)| match state {
                ManagedEnvironmentKind::Direct(direct) => direct.ssh_destination.map(|destination| (destination, direct.host_environment)),
                _ => None,
            })
        {
            // #3792: a session in a vessel attaches through the vessel's agent,
            // as a Docker vessel's does; the owning daemon resolves it.
            if let Some(access) = &self.agentless_vessels {
                let target = terminal_session_attach_target_with_stale_status(session)?;
                if let Some(attach) = access.attach(&session.spec, target.session_id, seat).await {
                    let (socket, name) = attach?;
                    return Ok((cleat_socket_attach_plan(&socket, &name, seat), self.host_name.clone()));
                }
            }
            let cwd = ExecutionEnvironmentPath::new(&session.spec.cwd);
            let attach_args = self.terminal_pool_attach_args(reads, session, &environment, &cwd, seat).await?;
            let plan = agentless_ssh_attach_plan(destination, &host_environment, attach_args);
            // The owning daemon resolves future attach requests; the target
            // has no flotillad to receive a recursive attach command.
            return Ok((plan, self.host_name.clone()));
        }
        let target_host = self.target_host_with_reads(reads, &namespace, host_ref).await?;
        if target_host != *self.host_name {
            let plan = self.recursive_attach_plan_for_remote(&target_host, reference, seat).await?;
            return Ok((plan, target_host));
        }

        let cwd = ExecutionEnvironmentPath::new(&session.spec.cwd);
        let args = self.terminal_pool_attach_args(reads, session, &environment, &cwd, seat).await?;
        Ok((ResolvedAttachPlan::command(args), self.host_name.clone()))
    }

    async fn recursive_attach_plan_for_remote(
        &self,
        target_host: &HostName,
        reference: &str,
        seat: AttachMode,
    ) -> Result<ResolvedAttachPlan, String> {
        let next_hop = self.host_registry.next_hop_host_for_target_host(target_host).await?.unwrap_or_else(|| target_host.clone());
        if next_hop == *self.host_name {
            return Err(format!("unreachable next hop for host '{target_host}': route points back to local host"));
        }

        let hosts = self.config.load_hosts()?;
        let connections = hosts
            .hosts
            .iter()
            .map(|(label, host)| {
                let target = match &host.user {
                    Some(user) => format!("{user}@{}", host.hostname),
                    None => host.hostname.clone(),
                };
                (HostName::new(&host.expected_host_name), SshConnection { target, multiplex: hosts.resolved_ssh_multiplex(label) })
            })
            .collect();
        let resolver = SshRemoteHopResolver::new(self.config.base_path().clone(), connections);
        let command = recursive_attach_command(target_host, reference, seat);
        resolver
            .one_hop_command_args(&next_hop, command)
            .map(ResolvedAttachPlan::command)
            .map_err(|err| format!("unreachable next hop '{next_hop}' for host '{target_host}': {err}"))
    }

    pub(super) async fn route_remote_attach_binding(&self, binding: &AttachBinding) -> Result<ResolvedAttachPlan, String> {
        let reference = binding
            .session
            .as_deref()
            .or(binding.convoy.as_deref())
            .ok_or_else(|| "remote attach binding has neither a session nor convoy reference".to_string())?;
        self.recursive_attach_plan_for_remote(&binding.host, reference, AttachMode::PreferTake).await
    }

    fn checked_provisioned_handle(
        &self,
        environment: &flotilla_resources::ResourceObject<ResourceEnvironment>,
    ) -> Result<Option<flotilla_environment::environment::EnvironmentHandle>, String> {
        let handle = self.environment_manager.managed_environments().into_iter().find_map(|(id, state)| {
            if id.as_str() != environment.metadata.name {
                return None;
            }
            match state {
                ManagedEnvironmentKind::Provisioned(state) => Some(state.handle),
                ManagedEnvironmentKind::Direct(_) => None,
            }
        });
        let Some(handle) = handle else {
            return Ok(None);
        };
        // The engine's checked identity is authoritative; no copied status or
        // provider discriminator can authorize a handle for a different backing.
        let realised = environment_attach_backing(self.lifecycle_reads, &environment.metadata.name)?;
        if handle.id().as_str() != environment.metadata.name || realised.as_ref().is_some_and(|backing| handle.backing_id() != backing) {
            return Err(format!("environment {} has no live handle for realised backing {realised:?}", environment.metadata.name));
        }
        Ok(Some(handle))
    }

    async fn provisioned_attach_plan_for_session(
        &self,
        reads: &AttachReadContext,
        session: &flotilla_resources::ResourceObject<ResourceTerminalSession>,
        environment: &flotilla_resources::ResourceObject<ResourceEnvironment>,
        handle: flotilla_environment::environment::EnvironmentHandle,
        seat: AttachMode,
    ) -> Result<ResolvedAttachPlan, String> {
        let cwd = ExecutionEnvironmentPath::new(&session.spec.cwd);
        let resident = async {
            let agent = handle.agent().ok_or("resident vessel agent is absent")?;
            agent.ready().await?;
            let registry = self.registry_for_resource_environment(reads, environment, cwd.as_path()).await?;
            let pool = registry.terminal_pools.get(&session.spec.pool).map(|(_, pool)| pool.clone()).ok_or("terminal pool unavailable")?;
            let endpoint = checked_cleat_endpoint(pool.as_ref(), session, seat).await?;
            let guest = Path::new(&endpoint.runtime_root).join(&endpoint.daemon).join("socket");
            let socket = agent.forward_socket(&guest).await?;
            Ok::<_, String>(cleat_socket_attach_plan(&socket, &endpoint.session, seat))
        }
        .await;
        let resident_error = match resident {
            Ok(plan) => return Ok(plan),
            Err(error) => error,
        };
        let terminal = async {
            let route = handle.terminal_route().ok_or("provider offers no authorized terminal route")?;
            // This capability supplies terminal-only access. It must not replace
            // the agent runner or mark the resident agent healthy.
            let bag = self.environment_manager.environment_bag(handle.id()).ok_or("terminal environment unavailable")?;
            let client = bag.find_binary("cleat").map(|binary| binary.as_path()).unwrap_or(Path::new("cleat"));
            let pool = flotilla_terminal::cleat::CleatTerminalPool::new(route.terminal_runner(), client.to_string_lossy(), &bag);
            let endpoint = checked_cleat_endpoint(&pool, session, seat).await?;
            route.attach_plan(&endpoint, client, seat)
        }
        .await;
        terminal.map_err(|error| {
            format!("resident vessel agent route unavailable: {resident_error}; provider terminal route unavailable: {error}")
        })
    }

    async fn terminal_pool_attach_args(
        &self,
        reads: &AttachReadContext,
        session: &flotilla_resources::ResourceObject<ResourceTerminalSession>,
        environment: &flotilla_resources::ResourceObject<ResourceEnvironment>,
        cwd: &ExecutionEnvironmentPath,
        seat: AttachMode,
    ) -> Result<Vec<Arg>, String> {
        let registry = self.registry_for_resource_environment(reads, environment, cwd.as_path()).await?;
        let pool = registry
            .terminal_pools
            .get(&session.spec.pool)
            .map(|(_, pool)| Arc::clone(pool))
            .ok_or_else(|| format!("terminal pool {} unavailable for environment {}", session.spec.pool, session.spec.env_ref))?;
        if session.status.as_ref().and_then(|status| status.session_id.as_ref()).is_none() {
            return Err(format!("session {} no longer exists on {}", session.metadata.name, self.host_name));
        }
        let attach_target = terminal_session_attach_target_with_stale_status(session)?;
        match pool.session_liveness(attach_target.session_id).await {
            Ok(TerminalSessionLiveness::Running) => {}
            Ok(TerminalSessionLiveness::Stopped | TerminalSessionLiveness::Absent) => {
                return Err(format!("session {} no longer exists on {}", session.metadata.name, self.host_name));
            }
            Ok(TerminalSessionLiveness::Lost(reason)) => {
                return Err(format!("session {} is unreachable on {}: {reason}", session.metadata.name, self.host_name));
            }
            Err(error) => return Err(format!("{} unreachable: {error}", self.host_name)),
        }
        pool.preflight_attach(seat).await?;
        pool.attach_args_for_mode(attach_target.session_id, attach_target.launch_command, cwd, &Vec::new(), seat)
    }

    #[cfg(test)]
    pub(super) async fn target_host_for_resource_ref(&self, namespace: &str, host_ref: &str) -> Result<HostName, String> {
        self.target_host_with_reads(&AttachReadContext::default(), namespace, host_ref).await
    }

    async fn target_host_with_reads(&self, reads: &AttachReadContext, namespace: &str, host_ref: &str) -> Result<HostName, String> {
        let canonical_ref = match reads.canonical_host(self.resource_backend, namespace, host_ref).await {
            Ok(Some(target_host)) => target_host.reference,
            Ok(None) => return Err(format!("references unknown host `{host_ref}`")),
            Err(error) => return Err(error),
        };
        Ok(self.host_name_for_canonical_ref(&canonical_ref))
    }

    async fn registry_for_resource_environment(
        &self,
        reads: &AttachReadContext,
        environment: &flotilla_resources::ResourceObject<ResourceEnvironment>,
        cwd: &Path,
    ) -> Result<Arc<ProviderRegistry>, String> {
        let environment_id = if let Some(host_direct) = environment.spec.host_direct.as_ref() {
            let canonical_ref =
                match reads.canonical_host(self.resource_backend, &environment.metadata.namespace, &host_direct.host_ref).await {
                    Ok(Some(target_host)) => target_host.reference,
                    Ok(None) => return Err(format!("references unknown host `{}`", host_direct.host_ref)),
                    Err(error) => return Err(error),
                };
            if self.canonical_local_host_id().as_ref() == Some(&canonical_ref) {
                self.local_environment_id.clone()
            } else {
                EnvironmentId::new(environment.metadata.name.clone())
            }
        } else {
            EnvironmentId::new(environment.metadata.name.clone())
        };

        if let Some(registry) = self.environment_registry_for_environment(&environment_id) {
            return Ok(registry);
        }

        discover_repo_for_environment(
            self.environment_manager,
            self.discovery,
            self.config,
            self.resource_backend,
            &self.provisioning_namespace().await,
            self.local_environment_id,
            &environment_id,
            cwd,
            None,
        )
        .await
        .map(|result| Arc::new(result.registry))
    }

    fn canonical_local_host_id(&self) -> Option<CanonicalHostId> {
        self.local_host_id().map(|host_id| CanonicalHostId::resolved(host_id.as_str()))
    }

    fn host_name_for_canonical_ref(&self, canonical_ref: &CanonicalHostId) -> HostName {
        if self.canonical_local_host_id().as_ref() == Some(canonical_ref) {
            self.host_name.clone()
        } else {
            HostName::new(canonical_ref.as_str())
        }
    }
}

use flotilla_projections::ports::ResolvedAttach;

/// Attach to a session on an agentless SSH host: the terminal pool's attach
/// command runs in the host's composed environment, never its login profile
/// (#3722).
fn agentless_ssh_attach_plan(destination: String, host_environment: &HostEnvironment, attach_args: Vec<Arg>) -> ResolvedAttachPlan {
    let mut remote = vec![Arg::Literal(host_environment.shell_prelude())];
    remote.extend(attach_args);
    ResolvedAttachPlan::command(vec![
        Arg::Literal("ssh".to_string()),
        Arg::Literal("-tt".to_string()),
        Arg::Literal("-o".to_string()),
        Arg::Literal("BatchMode=yes".to_string()),
        Arg::Quoted(destination),
        Arg::Literal("sh".to_string()),
        Arg::Literal("-c".to_string()),
        Arg::NestedCommand(remote),
    ])
}
fn attach_convoy_phase(phase: ResourceConvoyPhase) -> ConvoyPhase {
    match phase {
        ResourceConvoyPhase::Pending => ConvoyPhase::Pending,
        ResourceConvoyPhase::Active => ConvoyPhase::Active,
        ResourceConvoyPhase::Interrupted => ConvoyPhase::Interrupted,
        ResourceConvoyPhase::Landing => ConvoyPhase::Landing,
        ResourceConvoyPhase::Abandoned => ConvoyPhase::Abandoned,
        ResourceConvoyPhase::Landed => ConvoyPhase::Landed,
        ResourceConvoyPhase::Failed => ConvoyPhase::Failed,
    }
}

fn convoy_reference_query(references: &[String]) -> StoreQuery {
    let mut alternatives = Vec::new();
    for reference in references {
        let root = reference.split('/').next().unwrap_or(reference);
        alternatives.push(StoreQuery { field_prefixes: BTreeMap::from([("metadata.name".into(), root.into())]), ..Default::default() });
        let (role, project) = root.split_once('@').map_or((root, None), |(role, project)| (role, Some(project)));
        let mut fields = BTreeMap::new();
        let mut field_prefixes = BTreeMap::new();
        if project.is_some() {
            fields.insert("spec.role".into(), role.into());
        } else {
            field_prefixes.insert("spec.role".into(), role.into());
        }
        if let Some(project) = project {
            field_prefixes.insert("spec.project_ref".into(), project.into());
        }
        alternatives.push(StoreQuery { fields, field_prefixes, ..Default::default() });
    }
    StoreQuery { any_of: Some(alternatives), include_replicas: true, ..Default::default() }
}

fn session_reference_query(references: &[String]) -> StoreQuery {
    let mut alternatives = Vec::new();
    for reference in references {
        alternatives
            .push(StoreQuery { field_prefixes: BTreeMap::from([("metadata.name".into(), reference.clone())]), ..Default::default() });
        let parts: Vec<_> = reference.split('/').collect();
        let root = parts[0];
        for key in [CONVOY_LABEL, VESSEL_REF_LABEL] {
            let mut labels = BTreeMap::from([(key.into(), root.into())]);
            if let Some(role) = parts.get(2) {
                labels.insert(ROLE_LABEL.into(), (*role).into());
            }
            if key == CONVOY_LABEL {
                if let Some(vessel) = parts.get(1) {
                    labels.insert(VESSEL_LABEL.into(), (*vessel).into());
                }
            } else if let Some(role) = parts.get(1) {
                labels.insert(ROLE_LABEL.into(), (*role).into());
            }
            alternatives.push(StoreQuery { label_prefixes: labels, ..Default::default() });
        }
        if !root.contains('@') {
            // Fleet rows also expose within-convoy crew and environment aliases.
            // Select these aliases before decoding, just like durable session keys.
            if parts.len() <= 2 {
                let mut labels = BTreeMap::from([(VESSEL_LABEL.into(), root.into())]);
                if let Some(role) = parts.get(1) {
                    labels.insert(ROLE_LABEL.into(), (*role).into());
                }
                alternatives.push(StoreQuery { label_prefixes: labels, ..Default::default() });
            }
            if parts.len() == 1 {
                alternatives.push(StoreQuery { label_prefixes: BTreeMap::from([(ROLE_LABEL.into(), root.into())]), ..Default::default() });
                for field in ["spec.role", "spec.env_ref"] {
                    alternatives.push(StoreQuery { field_prefixes: BTreeMap::from([(field.into(), root.into())]), ..Default::default() });
                }
            }
        }
    }
    StoreQuery { any_of: Some(alternatives), include_replicas: true, ..Default::default() }
}

fn attach_reference_keys(session_name: &str, labels: &BTreeMap<String, String>, convoy_address: Option<&str>) -> Vec<String> {
    let mut refs = vec![session_name.to_string()];

    let convoy = labels.get(CONVOY_LABEL);
    let task = labels.get(VESSEL_LABEL);
    let role = labels.get(ROLE_LABEL);
    let vessel = flotilla_resources::label_value(labels, VESSEL_REF_LABEL);

    if let Some(convoy) = convoy {
        refs.push(convoy.clone());
    }
    if let Some(address) = convoy_address {
        refs.push(address.to_string());
        if let Some(task) = task {
            refs.push(format!("{address}/{task}"));
        }
        if let (Some(task), Some(role)) = (task, role) {
            refs.push(format!("{address}/{task}/{role}"));
        }
    }
    if let Some(vessel) = vessel {
        refs.push(vessel.clone());
    }
    if let (Some(convoy), Some(task)) = (convoy, task) {
        refs.push(format!("{convoy}/{task}"));
    }
    if let (Some(convoy), Some(task), Some(role)) = (convoy, task, role) {
        refs.push(format!("{convoy}/{task}/{role}"));
    }
    if let (Some(vessel), Some(role)) = (vessel, role) {
        refs.push(format!("{vessel}/{role}"));
    }
    if let Some(role) = role {
        refs.push(role.clone());
    }

    refs.sort();
    refs.dedup();
    refs
}

fn attach_reference_label(session_name: &str, labels: &BTreeMap<String, String>, convoy_address: Option<&str>) -> String {
    if let Some(address) = convoy_address {
        return format!("{} ({session_name})", address.replace('@', " @ "));
    }
    match (labels.get(CONVOY_LABEL), labels.get(VESSEL_LABEL), labels.get(ROLE_LABEL)) {
        (Some(convoy), Some(task), Some(role)) => format!("{convoy}/{task}/{role} ({session_name})"),
        (Some(convoy), Some(task), None) => format!("{convoy}/{task} ({session_name})"),
        (Some(convoy), None, Some(role)) => format!("{convoy}/{role} ({session_name})"),
        (Some(convoy), None, None) => format!("{convoy} ({session_name})"),
        _ => session_name.to_string(),
    }
}

enum AttachSessionSource {
    Durable,
    Observed,
}

type ConvoyAddresses = HashMap<(String, String, Option<flotilla_protocol::NodeId>), String>;

fn session_convoy_address<'a>(
    session: &flotilla_resources::ResourceObject<ResourceTerminalSession>,
    origin: Option<&flotilla_protocol::NodeId>,
    addresses: &'a ConvoyAddresses,
) -> Option<&'a String> {
    let name = session.metadata.labels.get(CONVOY_LABEL)?;
    if let Some(address) = addresses.get(&(session.metadata.namespace.clone(), name.clone(), origin.cloned())) {
        return Some(address);
    }
    // A placed session can belong to a convoy authored by another origin.
    // Resolve that cross-origin reference only when its address is unambiguous.
    let mut matches = addresses
        .iter()
        .filter(|((namespace, convoy, _), _)| namespace == &session.metadata.namespace && convoy == name)
        .map(|(_, address)| address);
    let address = matches.next()?;
    matches.all(|candidate| candidate == address).then_some(address)
}

fn replica_origin(provenance: &ResourceProvenance) -> Option<&flotilla_protocol::NodeId> {
    match provenance {
        ResourceProvenance::Local => None,
        ResourceProvenance::Replica { origin_root, .. } => Some(origin_root),
    }
}

fn fleet_row_attach_reference_keys(row: &FleetListRow) -> Vec<String> {
    let address = row.convoy.replace(" @ ", "@");
    let mut refs = vec![address.clone(), row.vessel.clone(), row.crew.clone()];
    if let Some(convoy_ref) = &row.convoy_ref {
        refs.push(convoy_ref.clone());
    }
    if let Some(session) = &row.session {
        refs.push(session.clone());
    }
    if row.crew != "-" {
        refs.push(format!("{address}/{}", row.crew));
        if let Some(convoy_ref) = &row.convoy_ref {
            refs.push(format!("{convoy_ref}/{}", row.crew));
        }
        if let Some((_task, role)) = row.crew.rsplit_once('/') {
            refs.push(role.to_string());
        }
    }
    refs.sort();
    refs.dedup();
    refs
}

fn fleet_row_attach_reference_label(row: &FleetListRow) -> String {
    if row.crew == "-" {
        format!("{} ({})", row.convoy, row.host)
    } else {
        format!("{}/{} ({})", row.convoy, row.crew, row.host)
    }
}

enum AttachTarget {
    Local(Box<flotilla_resources::ResourceObject<ResourceTerminalSession>>),
    Replica { row: Box<FleetListRow> },
    Checkout(Box<CheckoutRow>),
}

impl AttachTarget {
    async fn resolve(
        &self,
        resolver: &AttachResolver<'_>,
        reads: &AttachReadContext,
        reference: &str,
        transient: bool,
        seat: AttachMode,
    ) -> Result<ResolvedAttach, String> {
        match self {
            Self::Local(session) => {
                let (plan, host) = resolver.attach_plan_for_session(reads, reference, session, seat).await?;
                let labels = &session.metadata.labels;
                let binding = AttachBinding::builder()
                    .host(host)
                    .namespace(session.metadata.namespace.clone())
                    .session(session.metadata.name.clone())
                    .maybe_convoy(labels.get(CONVOY_LABEL).cloned())
                    .maybe_vessel(labels.get(VESSEL_LABEL).cloned())
                    .role(labels.get(ROLE_LABEL).cloned().unwrap_or_else(|| session.spec.role.clone()))
                    .build();
                Ok(ResolvedAttach { plan, binding: Some(binding) })
            }
            Self::Replica { row } => {
                let plan = resolver.recursive_attach_plan_for_remote(&row.host, reference, seat).await?;
                // Replica rows carry crew as "vessel/role" (or a bare role)
                // and the owning host's namespace + session name, so
                // cross-host panes stamp the full join key.
                let (vessel, role) = match row.crew.split_once('/') {
                    Some((vessel, role)) => (Some(vessel.to_owned()), Some(role.to_owned())),
                    None => (None, Some(row.crew.clone()).filter(|role| !role.is_empty() && role != "-")),
                };
                let binding = AttachBinding::builder()
                    .host(row.host.clone())
                    .namespace(row.namespace.clone())
                    .maybe_session(row.session.clone())
                    .maybe_convoy(row.convoy_ref.clone().or_else(|| Some(row.convoy.clone()).filter(|convoy| convoy != "-")))
                    .maybe_vessel(vessel)
                    .maybe_role(role)
                    .build();
                Ok(ResolvedAttach { plan, binding: Some(binding) })
            }
            Self::Checkout(row) => {
                if !transient {
                    return Err(format!("checkout '{}' is only available as a transient attach target", row.path));
                }
                let plan = if row.host == *resolver.host_name {
                    resolver.local_checkout_terminal_plan(row, seat).await?
                } else {
                    resolver.recursive_attach_plan_for_remote(&row.host, reference, seat).await?
                };
                // The viewer needs the selected checkout host to build its
                // own SSH route. A transient checkout has no durable session
                // identity, so preserve the checkout reference rather than
                // exposing its terminal-pool session name.
                let binding = AttachBinding::builder().host(row.host.clone()).namespace(row.resource.namespace.clone()).build();
                Ok(ResolvedAttach { plan, binding: Some(binding) })
            }
        }
    }
}

struct AttachCandidate {
    label: String,
    references: Vec<String>,
    host: HostName,
    target: AttachTarget,
}

struct AttachCandidateIndex {
    reads: AttachReadContext,
    convoys: Vec<flotilla_resources::ReadResourceObject<ResourceConvoy>>,
    sessions: Vec<flotilla_resources::ReadResourceObject<ResourceTerminalSession>>,
    candidates: Vec<AttachCandidate>,
    exact: HashMap<String, Vec<usize>>,
}

enum AttachIndexError {
    NoMatch(String),
    Other(String),
}

impl AttachIndexError {
    fn into_message(self) -> String {
        match self {
            Self::NoMatch(message) | Self::Other(message) => message,
        }
    }
}

impl AttachCandidateIndex {
    fn new(
        candidates: Vec<AttachCandidate>,
        convoys: Vec<flotilla_resources::ReadResourceObject<ResourceConvoy>>,
        sessions: Vec<flotilla_resources::ReadResourceObject<ResourceTerminalSession>>,
    ) -> Self {
        let mut exact: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, candidate) in candidates.iter().enumerate() {
            for reference in &candidate.references {
                exact.entry(reference.clone()).or_default().push(index);
            }
        }
        Self { reads: AttachReadContext::default(), candidates, exact, convoys, sessions }
    }

    async fn resolve(
        &self,
        resolver: &AttachResolver<'_>,
        reference: &str,
        host: Option<&HostName>,
        transient: bool,
        seat: AttachMode,
    ) -> Result<ResolvedAttach, AttachIndexError> {
        if reference.trim().is_empty() {
            return Err(AttachIndexError::Other("attach reference is required".to_string()));
        }

        let mut matches = self.exact.get(reference).cloned().unwrap_or_else(|| {
            self.candidates
                .iter()
                .enumerate()
                .filter(|(_, candidate)| candidate.references.iter().any(|candidate_reference| candidate_reference.starts_with(reference)))
                .map(|(index, _)| index)
                .collect()
        });
        if let Some(host) = host {
            matches.retain(|index| &self.candidates[*index].host == host);
        }
        match matches.as_slice() {
            [] => match host {
                Some(host) => Err(AttachIndexError::NoMatch(format!("no attach target matching '{reference}' on host '{host}'"))),
                None => Err(AttachIndexError::NoMatch(format!("no attach target matching '{reference}'"))),
            },
            [index] => {
                let mut resolved = self.candidates[*index]
                    .target
                    .resolve(resolver, &self.reads, reference, transient, seat)
                    .await
                    .map_err(AttachIndexError::Other)?;
                if let Some(binding) = resolved.binding.as_mut() {
                    if let Some(name) = binding.convoy.as_ref() {
                        let observed = self.convoys.iter().find(|source| source.object.metadata.name == *name);
                        binding.convoy_phase = if let Some(source) = observed {
                            source.object.status.as_ref().map(|status| attach_convoy_phase(status.phase))
                        } else {
                            // A direct TerminalSession name already identifies its parent;
                            // fetch that exact name rather than scanning Convoys for phase.
                            let namespace = resolver.provisioning_namespace().await;
                            match resolver.resource_backend.including_replicas::<ResourceConvoy>(&namespace).get(name).await {
                                Ok(source) => source.object.status.map(|status| attach_convoy_phase(status.phase)),
                                Err(flotilla_resources::ResourceError::NotFound { .. }) => None,
                                Err(error) => return Err(AttachIndexError::Other(error.to_string())),
                            }
                        };
                    }
                }
                Ok(resolved)
            }
            _ => {
                let mut labels: Vec<_> = matches.iter().map(|index| self.candidates[*index].label.clone()).collect();
                labels.sort();
                labels.dedup();
                Err(AttachIndexError::Other(format!("attach reference '{reference}' is ambiguous: {}", labels.join(", "))))
            }
        }
    }
}

fn recursive_attach_command(target_host: &HostName, reference: &str, seat: AttachMode) -> Vec<flotilla_protocol::arg::Arg> {
    let mut command =
        vec![flotilla_protocol::arg::Arg::Literal("flotilla".to_string()), flotilla_protocol::arg::Arg::Literal("attach".to_string())];
    command.push(flotilla_protocol::arg::Arg::Literal("--host".to_string()));
    command.push(flotilla_protocol::arg::Arg::Quoted(target_host.to_string()));
    // Recursive attaches only traverse transport boundaries; Presentation
    // Manager identity belongs to the original foreground attach.
    command.push(flotilla_protocol::arg::Arg::Literal("--transient".to_string()));
    match seat {
        AttachMode::Default => command.push(flotilla_protocol::arg::Arg::Literal("--watch".to_string())),
        AttachMode::PreferTake => {}
        AttachMode::Strict => command.push(flotilla_protocol::arg::Arg::Literal("--strict".to_string())),
        AttachMode::Take => command.push(flotilla_protocol::arg::Arg::Literal("--take".to_string())),
    }
    command.push(flotilla_protocol::arg::Arg::Quoted(reference.to_string()));
    command
}

fn transient_checkout_session_name(checkout: &CheckoutRow) -> String {
    let mut hasher = Sha256::new();
    hasher.update(checkout.host.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(checkout.path.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("flotilla-checkout-{}", &digest[..32])
}

/// Both authorized routes target the same recorded physical generation. An ID
/// found on a different daemon is not permission to attach to that daemon.
async fn checked_cleat_endpoint(
    pool: &dyn flotilla_terminal::TerminalPool,
    session: &flotilla_resources::ResourceObject<ResourceTerminalSession>,
    seat: AttachMode,
) -> Result<flotilla_protocol::result_set::CleatEndpoint, String> {
    let target = terminal_session_attach_target_with_stale_status(session)?;
    let recorded = session
        .status
        .as_ref()
        .and_then(|status| status.cleat_endpoint.as_ref())
        .ok_or("selected terminal has no recorded Cleat endpoint")?;
    if recorded.session != target.session_id
        || !Path::new(&recorded.runtime_root).is_absolute()
        || recorded.daemon.contains('/')
        || recorded
            .daemon
            .rsplit_once('@')
            .and_then(|(_, generation)| generation.parse::<u64>().ok())
            .is_none_or(|generation| generation == 0)
    {
        return Err("selected terminal has a mismatched Cleat session or generation".into());
    }
    match pool.session_liveness(target.session_id).await? {
        TerminalSessionLiveness::Running => {}
        state => return Err(format!("selected terminal is not live: {state:?}")),
    }
    pool.preflight_attach(seat).await?;
    pool.attach_args_for_mode(
        target.session_id,
        target.launch_command,
        &ExecutionEnvironmentPath::new(&session.spec.cwd),
        &Vec::new(),
        seat,
    )?;
    let current = pool.cleat_endpoint(target.session_id).await?.ok_or("selected terminal has no unique live Cleat endpoint")?;
    if current != *recorded {
        return Err("selected terminal Cleat endpoint/generation no longer matches current inventory".into());
    }
    Ok(current)
}

fn cleat_socket_attach_plan(socket: &Path, session: &str, seat: AttachMode) -> ResolvedAttachPlan {
    let mut args = vec![
        Arg::Literal("cleat".into()),
        Arg::Literal("attach".into()),
        Arg::Literal("--socket".into()),
        Arg::Quoted(socket.to_string_lossy().into_owned()),
        Arg::Literal("--no-create".into()),
    ];
    match seat {
        AttachMode::Default => {}
        AttachMode::Strict => args.push(Arg::Literal("--strict".into())),
        AttachMode::PreferTake | AttachMode::Take => args.push(Arg::Literal("--take".into())),
    }
    args.push(Arg::Quoted(session.into()));
    ResolvedAttachPlan::command(args)
}

#[cfg(test)]
mod tests {
    use flotilla_store_testkit::ReadCountsBackendExt;
    use std::path::PathBuf;

    use chrono::Utc;
    use flotilla_protocol::{Command, CommandAction, CommandValue};
    use flotilla_resources::{
        ConvoyPhase, ConvoySpec, ConvoyStatus, Host as ResourceHost, HostSpec, TerminalSessionPhase as ResourceTerminalSessionPhase,
    };

    use super::*;
    use crate::in_process::tests::create_identity_convoy;
    use crate::in_process::tests::create_running_session;
    use crate::in_process::tests::create_test_environment;
    use crate::in_process::tests::test_meta;
    use crate::in_process::InProcessDaemon;
    use crate::testkits::discovery::fake_discovery;
    use flotilla_config::ConfigStore;
    use flotilla_daemon_api::daemon::DaemonHandle;

    // Attach uses the engine identity, refuses every non-ready held state, and propagates read errors.
    // Every old phase is stale; repeated reads cannot create a copy or resurrect a stale handle.
    #[tokio::test]
    async fn attach_reads_environment_backing_without_a_copy() {
        use flotilla_provisioning::engine::environment::EnvironmentBinding;
        use flotilla_provisioning_testkit::{object, Ending, LifecycleFixture};
        use flotilla_resources::{EnvironmentPhase, EnvironmentSpec, EnvironmentStatus, HostDirectEnvironmentSpec};
        let obj = object::<ResourceEnvironment>(
            "env",
            EnvironmentSpec {
                host_direct: Some(HostDirectEnvironmentSpec { host_ref: "host".into(), repo_default_dir: "/work".into() }),
                ..Default::default()
            },
            EnvironmentStatus { phase: EnvironmentPhase::Failed, docker_container_id: Some("stale".into()), ..Default::default() },
        );
        let fixture = LifecycleFixture::<EnvironmentBinding>::new(false);
        let read = || environment_attach_backing(fixture.reads.as_ref(), "env");
        assert_eq!(read().expect("unheld"), None);
        fixture.request(&obj);
        assert!(read().is_err());
        fixture.finish(&obj, Ending::Ready("live-backing"));
        assert_eq!(read().expect("ready"), Some("live-backing".into()));
        fixture.finish(&obj, Ending::Lost("live-backing"));
        assert!(read().expect_err("lost").contains("observed absent"));
        fixture.poison();
        assert!(read().expect_err("read error").contains("poisoned"));
    }

    // #3722: attach to an agentless session runs its command in the host's
    // composed environment. The plan's remote part runs here as sshd runs it,
    // with a login profile that must not be read.
    #[cfg(unix)]
    #[test]
    fn agentless_attach_composes_the_host_environment_without_its_login_profile() {
        use flotilla_protocol::{arg::flatten, attach_plan::ResolvedAttachAction};
        use flotilla_provider_runtime::host_environment::HostSupply;

        let home = tempfile::tempdir().expect("remote home");
        std::fs::write(home.path().join(".profile"), "echo read > \"$HOME/profile-read\"\n").expect("login profile");
        let host =
            HostEnvironment::new(vec![HostSupply { name: "cleat".into(), path: vec!["~/.local/opt/cleat/bin".into()], env: Vec::new() }])
                .expect("declared supply");
        let plan = agentless_ssh_attach_plan("crew@b1".into(), &host, vec![Arg::Literal("printf '%s' \"$PATH\"".into())]);
        let [ResolvedAttachAction::Command(args)] = plan.0.as_slice() else { panic!("one command: {plan:?}") };
        assert_eq!(flatten(&args[..5], 0), "ssh -tt -o BatchMode=yes 'crew@b1'");
        // sshd hands the rest of the command line to the user's shell.
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &flatten(&args[5..], 0)])
            .env_clear()
            .env("HOME", home.path())
            .output()
            .expect("remote shell");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(
            String::from_utf8(output.stdout).expect("utf-8"),
            format!("{}/.local/opt/cleat/bin:/usr/local/bin:/usr/bin:/bin", home.path().display())
        );
        assert!(!home.path().join("profile-read").exists(), "attach read the login profile");
    }

    // The vessel hop executes the host cleat client and forwards only a socket.
    // Generate every seat mode plus socket/session names needing shell quoting.
    #[hegel::test]
    fn vessel_attach_plan_preserves_socket_session_and_seat(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let modes = [AttachMode::Default, AttachMode::PreferTake, AttachMode::Strict, AttachMode::Take];
        let mode = modes[tc.draw(gs::integers::<usize>().min_value(0).max_value(3))];
        let names = ["plain", "with spaces", "with'quote", "Unicode-λ"];
        let name = names[tc.draw(gs::integers::<usize>().min_value(0).max_value(3))];
        let socket = PathBuf::from(format!("/tmp/{name}/socket"));
        let plan = cleat_socket_attach_plan(&socket, name, mode);
        let [ResolvedAttachAction::Command(args)] = plan.0.as_slice() else {
            panic!("one hop");
        };
        assert_eq!(args[0], Arg::Literal("cleat".into()));
        assert!(args.windows(2).any(|pair| pair == [Arg::Literal("--socket".into()), Arg::Quoted(socket.to_string_lossy().into_owned())]));
        assert_eq!(args.last(), Some(&Arg::Quoted(name.into())));
        assert_eq!(args.contains(&Arg::Literal("--take".into())), matches!(mode, AttachMode::PreferTake | AttachMode::Take));
        assert_eq!(args.contains(&Arg::Literal("--strict".into())), mode == AttachMode::Strict);
        assert!(!args.contains(&Arg::Literal("docker".into())));
    }

    // Attach scenarios need a daemon and providers, independent of standing-convoy admission.
    async fn attach_fixture() -> (Arc<InProcessDaemon>, ResourceBackend, tempfile::TempDir) {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("daemon.toml"), "machine_id = \"attach-test\"\n").expect("daemon config");
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        let daemon = InProcessDaemon::new_with_resource_backend(
            Vec::new(),
            Arc::new(ConfigStore::with_base(temp.path())),
            fake_discovery(false),
            HostName::new("local"),
            backend.clone(),
            std::sync::Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
        )
        .await;
        (daemon, backend, temp)
    }

    // A pre-agent backing is adopted from inventory, never newly provisioned.
    // Normal attach must preserve its selected live Cleat generation after readoption.
    #[tokio::test]
    async fn adopted_pre_agent_terminal_remains_attachable() {
        adopted_terminal_contract(None, ContractProvider::Docker).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires an installed Cleat with controller-seat support and a real PTY"]
    async fn adopted_terminal_preserves_real_cleat_process() {
        let mut real = Arc::new(RealTerminal::new());
        adopted_terminal_contract(Some(real.clone()), ContractProvider::Docker).await;
        Arc::get_mut(&mut real).expect("all attach consumers released").native.shutdown();
    }

    // A provider with neither a Docker spec nor a runtime container name must
    // use the same normal attach contract solely through its explicit capability.
    #[tokio::test]
    async fn non_docker_provisioned_terminal_uses_explicit_route() {
        adopted_terminal_contract(None, ContractProvider::Independent).await;
    }

    #[derive(Clone, Copy)]
    enum ContractProvider {
        Docker,
        Independent,
    }

    struct RealTerminal {
        endpoint: flotilla_protocol::result_set::CleatEndpoint,
        baseline: HashMap<String, String>,
        #[cfg(target_os = "linux")]
        native: flotilla_discovery_testkit::native_terminal::NativeTerminal,
    }

    // The opt-in child is this compiled Rust test, never an interpreted program.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "private native crew subprocess only"]
    fn native_crew_probe() {
        flotilla_discovery_testkit::native_terminal::crew_probe();
    }

    impl RealTerminal {
        #[cfg(target_os = "linux")]
        fn new() -> Self {
            let native = flotilla_discovery_testkit::native_terminal::NativeTerminal::new("in_process::attach::tests::native_crew_probe");
            Self { endpoint: native.endpoint.clone(), baseline: native.baseline.clone(), native }
        }

        fn attach(&self, plan: &ResolvedAttachPlan, secondary: &ResolvedAttachPlan) {
            let commands: Vec<Vec<String>> = [plan, secondary]
                .into_iter()
                .map(|plan| {
                    let [ResolvedAttachAction::Command(args)] = plan.0.as_slice() else { panic!("single provider hop") };
                    // Substitute only the Docker process boundary; execute the emitted
                    // Cleat command against the selected physical session unchanged.
                    args.iter()
                        .skip(4)
                        .map(|arg| match arg {
                            Arg::Literal(value) | Arg::Quoted(value) => value.clone(),
                            _ => panic!("plain provider arguments"),
                        })
                        .collect()
                })
                .collect();
            #[cfg(target_os = "linux")]
            self.native.attach(&commands);
            #[cfg(not(target_os = "linux"))]
            {
                let _ = commands;
                panic!("native acceptance is Linux-only");
            }
        }
    }

    async fn adopted_terminal_contract(real: Option<Arc<RealTerminal>>, provider_kind: ContractProvider) {
        use flotilla_discovery_api::EnvironmentBag;
        use flotilla_environment::environment::{
            docker::DockerEnvironmentProvider, EnvironmentHandle, EnvironmentProvider, EnvironmentTool, ProviderTerminalRoute,
            ProvisionedEnvironment, VesselAgent,
        };
        use flotilla_provider_runtime::{ChannelLabel, CommandOutput, CommandRunner};

        // The resident process boundary can fail independently of the provider transport.
        struct Resident(Result<(), String>);
        #[async_trait::async_trait]
        impl VesselAgent for Resident {
            async fn ready(&self) -> Result<(), String> {
                self.0.clone()
            }
            async fn forward_socket(&self, _: &Path) -> Result<PathBuf, String> {
                Ok("/forwarded/socket".into())
            }
            async fn push_tools(&self, _: &[EnvironmentTool]) -> Result<(), String> {
                panic!("attach must not bootstrap")
            }
        }
        enum Permission {
            Terminal,
            ShellOnly,
        }
        struct Access {
            inner: EnvironmentHandle,
            resident: Arc<dyn VesselAgent>,
            permission: Permission,
        }
        #[async_trait::async_trait]
        impl ProvisionedEnvironment for Access {
            fn backing_id(&self) -> &str {
                self.inner.backing_id()
            }
            fn capabilities(&self) -> flotilla_environment::environment::ProviderCapabilities {
                self.inner.capabilities()
            }
            fn id(&self) -> &EnvironmentId {
                self.inner.id()
            }
            fn image(&self) -> &flotilla_protocol::ImageId {
                self.inner.image()
            }
            async fn observe(&self) -> flotilla_environment::environment::ProviderObservation {
                self.inner.observe().await
            }
            async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
                self.inner.env_vars().await
            }
            fn runner(&self) -> Arc<dyn CommandRunner> {
                self.inner.runner()
            }
            fn agent(&self) -> Option<Arc<dyn VesselAgent>> {
                Some(self.resident.clone())
            }
            fn fallback_runner(&self) -> Option<Arc<dyn CommandRunner>> {
                self.inner.fallback_runner()
            }
            fn terminal_route(&self) -> Option<&dyn ProviderTerminalRoute> {
                match self.permission {
                    Permission::Terminal => self.inner.terminal_route(),
                    Permission::ShellOnly => None,
                }
            }
            async fn destroy(&self) -> Result<(), String> {
                panic!("attach must preserve the backing")
            }
        }

        // An independent adapter at the provider seam: no Docker spec, command,
        // container name or wrapped Docker handle participates in this route.
        struct Independent {
            id: EnvironmentId,
            image: flotilla_protocol::ImageId,
            runner: Arc<dyn CommandRunner>,
        }
        #[async_trait::async_trait]
        impl ProvisionedEnvironment for Independent {
            fn id(&self) -> &EnvironmentId {
                &self.id
            }
            fn image(&self) -> &flotilla_protocol::ImageId {
                &self.image
            }
            fn backing_id(&self) -> &str {
                "backing"
            }
            fn capabilities(&self) -> flotilla_environment::environment::ProviderCapabilities {
                flotilla_discovery_testkit::environment::injected_capabilities()
            }
            async fn observe(&self) -> flotilla_environment::environment::ProviderObservation {
                flotilla_environment::environment::ProviderObservation::new(flotilla_environment::environment::ObservationState::Running)
            }
            async fn env_vars(&self) -> Result<HashMap<String, String>, String> {
                Ok(HashMap::new())
            }
            fn runner(&self) -> Arc<dyn CommandRunner> {
                self.runner.clone()
            }
            fn fallback_runner(&self) -> Option<Arc<dyn CommandRunner>> {
                Some(self.runner.clone())
            }
            fn terminal_route(&self) -> Option<&dyn ProviderTerminalRoute> {
                Some(self)
            }
            async fn destroy(&self) -> Result<(), String> {
                panic!("attach must preserve independent backing")
            }
        }
        impl ProviderTerminalRoute for Independent {
            fn terminal_runner(&self) -> Arc<dyn CommandRunner> {
                self.runner.clone()
            }
            fn attach_plan(
                &self,
                endpoint: &flotilla_protocol::result_set::CleatEndpoint,
                client: &Path,
                seat: AttachMode,
            ) -> Result<ResolvedAttachPlan, String> {
                let socket = Path::new(&endpoint.runtime_root).join(&endpoint.daemon).join("socket");
                let ResolvedAttachPlan(actions) = cleat_socket_attach_plan(&socket, &endpoint.session, seat);
                let [ResolvedAttachAction::Command(mut args)] = actions.try_into().expect("one terminal command");
                args[0] = Arg::Quoted(client.to_string_lossy().into_owned());
                let mut transport = vec![
                    Arg::Literal("provider-terminal".into()),
                    Arg::Literal("connect".into()),
                    Arg::Literal("--backing".into()),
                    Arg::Quoted(self.backing_id().into()),
                ];
                transport.extend(args);
                Ok(ResolvedAttachPlan::command(transport))
            }
        }

        // Fake only the runtime process boundary, including Cleat inventory.
        struct Inventory(Option<Arc<RealTerminal>>, Arc<std::sync::atomic::AtomicBool>);
        #[async_trait::async_trait]
        impl CommandRunner for Inventory {
            async fn run(&self, _: &str, args: &[&str], _: &Path, _: &ChannelLabel) -> Result<String, String> {
                if args.first() == Some(&"ps") {
                    return Ok("backing work\tpre-agent\tbase\t[]".into());
                }
                if !self.1.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err("provider terminal transport unreachable".into());
                }
                if let Some(real) = &self.0 {
                    if args.contains(&"daemons") {
                        return Ok(
                            serde_json::json!([{"name": real.endpoint.daemon, "runtime_root": real.endpoint.runtime_root, "alive": true}])
                                .to_string(),
                        );
                    }
                    let command = if args.get(1) == Some(&"-w") { &args[4..] } else { &args[2..] };
                    return flotilla_provider_runtime::ProcessCommandRunner
                        .run(command[0], &command[1..], Path::new("/"), &ChannelLabel::Default)
                        .await;
                }
                if args.contains(&"daemons") {
                    return Ok(r#"[{"name":"crew@7","runtime_root":"/tmp/cleat","alive":true}]"#.into());
                }
                if args.contains(&"list") {
                    return Ok(r#"[{"id":"session-crew","status":"Detached"}]"#.into());
                }
                if args.contains(&"--help") {
                    return Ok("--strict --take".into());
                }
                panic!("unexpected runtime mutation or command: {args:?}");
            }
            async fn run_output(&self, _: &str, _: &[&str], _: &Path, _: &ChannelLabel) -> Result<CommandOutput, String> {
                unreachable!()
            }
            async fn exists(&self, _: &str, _: &[&str]) -> bool {
                false
            }
        }
        let (mut daemon, backend, temp) = attach_fixture().await;
        let host = daemon.local_host_id().unwrap().to_string();
        backend.using::<ResourceHost>("flotilla").create(&test_meta(&host), &HostSpec::default()).await.unwrap();
        let spec = match provider_kind {
            ContractProvider::Docker => serde_json::from_value(serde_json::json!({"docker": {"host_ref": host, "image": "base"}})).unwrap(),
            ContractProvider::Independent => flotilla_resources::EnvironmentSpec {
                provider_address: Some("account:isolated/provider:independent".parse().expect("provider address")),
                ..Default::default()
            },
        };
        let environments = backend.using::<ResourceEnvironment>("flotilla");
        let environment = environments.create(&test_meta("pre-agent"), &spec).await.unwrap();
        environments
            .update_status(
                "pre-agent",
                &environment.metadata.resource_version,
                &flotilla_resources::EnvironmentStatus {
                    docker_container_id: Some("backing".into()),
                    message: Some("resident vessel agent unavailable; recovery required".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        create_running_session(&daemon, "pre-agent", "crew", "convoy", "coder").await;
        let terminals = backend.using::<ResourceTerminalSession>("flotilla");
        let mut session = terminals.get("crew").await.unwrap();
        let endpoint = real.as_ref().map(|real| real.endpoint.clone()).unwrap_or(flotilla_protocol::result_set::CleatEndpoint {
            runtime_root: "/tmp/cleat".into(),
            daemon: "crew@7".into(),
            session: "session-crew".into(),
        });
        session.status.as_mut().unwrap().cleat_endpoint = Some(endpoint.clone());
        terminals.update_status("crew", &session.metadata.resource_version, session.status.as_ref().unwrap()).await.unwrap();

        for round in 0..2 {
            if round > 0 {
                daemon = InProcessDaemon::new_with_resource_backend(
                    Vec::new(),
                    Arc::new(ConfigStore::with_base(temp.path())),
                    fake_discovery(false),
                    HostName::new("local"),
                    backend.clone(),
                    Arc::new(flotilla_discovery_testkit::HostPeerOverlay::default()),
                )
                .await;
            }
            let available = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let runner = Arc::new(Inventory(real.clone(), available.clone()));
            let provider = match provider_kind {
                ContractProvider::Docker => Some(DockerEnvironmentProvider::new(runner.clone())),
                ContractProvider::Independent => None,
            };
            let handle: EnvironmentHandle = match &provider {
                Some(provider) => provider.list().await.unwrap().pop().unwrap(),
                None => {
                    Arc::new(Independent { id: EnvironmentId::new("pre-agent"), image: flotilla_protocol::ImageId::new("base"), runner })
                }
            };
            assert!(handle.agent().is_none(), "attachment must not bootstrap the resident agent");
            assert!(handle.provisioned_mounts().is_empty());
            let bag = real.as_ref().map(|real| EnvironmentBag::provisioned(real.baseline.clone())).unwrap_or_else(|| {
                EnvironmentBag::new().with(flotilla_discovery_api::EnvironmentAssertion::binary("cleat", "/declared/cleat"))
            });
            let pool = Arc::new(flotilla_terminal::cleat::CleatTerminalPool::new(handle.runner(), "cleat", &bag));
            let mut registry = ProviderRegistry::new();
            registry.terminal_pools.insert(
                "passthrough",
                flotilla_discovery_api::ProviderDescriptor::named(flotilla_discovery_api::ProviderCategory::TerminalPool, "cleat"),
                pool,
            );
            daemon
                .environment_manager
                .register_provisioned_environment(handle.id().clone(), handle.clone(), bag.clone(), Some(Arc::new(registry)))
                .unwrap();
            let plan = daemon
                .attach_resolver()
                .resolve_attach("crew", None, false, AttachMode::Strict, None)
                .await
                .expect("adopted live terminal")
                .plan;
            let rendered = format!("{plan:?}");
            let [ResolvedAttachAction::Command(args)] = plan.0.as_slice() else { panic!("one provider hop") };
            assert_eq!(args.get(3), Some(&Arg::Quoted("backing".into())), "transport targets the immutable backing, not its reusable name");
            assert!(
                rendered.contains(&endpoint.daemon)
                    && rendered.contains("session-crew")
                    && rendered.contains("--strict")
                    && rendered.contains("--no-create"),
                "{rendered}"
            );
            if let Some(real) = &real {
                let secondary = daemon
                    .attach_resolver()
                    .resolve_attach("crew", None, false, AttachMode::Default, None)
                    .await
                    .expect("default seat route")
                    .plan;
                real.attach(&plan, &secondary);
            }
            assert!(handle.agent().is_none(), "successful degraded attach does not repair agent health");
            assert_eq!(
                environments.get("pre-agent").await.unwrap().status.as_ref().and_then(|status| status.message.as_deref()),
                Some("resident vessel agent unavailable; recovery required"),
                "degraded terminal access preserves the resident-agent recovery diagnostic"
            );

            struct BackingReading(&'static str);
            impl flotilla_projections::ports::LifecycleReads for BackingReading {
                fn try_reading(&self, kind: ProvisioningKind, _: &str, _: Option<&str>) -> Result<Option<LifecycleReading>, String> {
                    Ok((kind == ProvisioningKind::Environment).then(|| LifecycleReading::Ready { identity: self.0.into() }))
                }
            }
            let ready = BackingReading("backing");
            let mut held = daemon.attach_resolver();
            held.lifecycle_reads = &ready;
            held.resolve_attach("crew", None, false, AttachMode::Default, None).await.expect("current engine identity");
            let engine = BackingReading("current-backing");
            let mut resolver = daemon.attach_resolver();
            resolver.lifecycle_reads = &engine;
            let error = resolver.resolve_attach("crew", None, false, AttachMode::Default, None).await.unwrap_err();
            assert!(error.contains("current-backing"), "the engine's realised identity overrides older resource status: {error}");

            // Copied legacy status cannot override the realised identity; a changed physical endpoint revokes access.
            let mut environment = environments.get("pre-agent").await.unwrap();
            let original = environment.status.clone().unwrap();
            environment.status.as_mut().unwrap().docker_container_id = Some("replaced-backing".into());
            let updated = environments
                .update_status("pre-agent", &environment.metadata.resource_version, environment.status.as_ref().unwrap())
                .await
                .unwrap();
            held.resolve_attach("crew", None, false, AttachMode::Default, None).await.expect("stale copied status is ignored");
            environments.update_status("pre-agent", &updated.metadata.resource_version, &original).await.unwrap();
            for wrong in [
                flotilla_protocol::result_set::CleatEndpoint { session: "another-terminal".into(), ..endpoint.clone() },
                flotilla_protocol::result_set::CleatEndpoint { daemon: "crew@6".into(), ..endpoint.clone() },
                flotilla_protocol::result_set::CleatEndpoint { runtime_root: "/another/root".into(), ..endpoint.clone() },
            ] {
                let mut current = terminals.get("crew").await.unwrap();
                current.status.as_mut().unwrap().cleat_endpoint = Some(wrong);
                terminals.update_status("crew", &current.metadata.resource_version, current.status.as_ref().unwrap()).await.unwrap();
                assert!(daemon.attach_resolver().resolve_attach("crew", None, false, AttachMode::Default, None).await.is_err());
            }
            let mut current = terminals.get("crew").await.unwrap();
            current.status.as_mut().unwrap().cleat_endpoint = Some(endpoint.clone());
            terminals.update_status("crew", &current.metadata.resource_version, current.status.as_ref().unwrap()).await.unwrap();

            // The same authorization, binding and seat contract applies with a
            // separately unreachable resident and with a healthy preferred route.
            for (resident, permission, expected) in [
                (
                    Resident(Err("resident unreachable".into())),
                    Permission::Terminal,
                    match provider_kind {
                        ContractProvider::Docker => "docker",
                        ContractProvider::Independent => "provider-terminal",
                    },
                ),
                (Resident(Ok(())), Permission::Terminal, "/forwarded/socket"),
                (Resident(Err("resident unreachable".into())), Permission::ShellOnly, "no authorized terminal route"),
            ] {
                let access = Arc::new(Access { inner: handle.clone(), resident: Arc::new(resident), permission });
                let mut registry = ProviderRegistry::new();
                registry.terminal_pools.insert(
                    "passthrough",
                    flotilla_discovery_api::ProviderDescriptor::named(flotilla_discovery_api::ProviderCategory::TerminalPool, "cleat"),
                    Arc::new(flotilla_terminal::cleat::CleatTerminalPool::new(handle.runner(), "cleat", &bag)),
                );
                daemon
                    .environment_manager
                    .register_provisioned_environment(handle.id().clone(), access.clone(), bag.clone(), Some(Arc::new(registry)))
                    .unwrap();
                for mode in [AttachMode::Default, AttachMode::Strict, AttachMode::PreferTake, AttachMode::Take] {
                    let result = daemon.attach_resolver().resolve_attach("crew", None, false, mode, None).await;
                    if expected == "no authorized terminal route" {
                        let error = result.unwrap_err();
                        assert!(error.contains("resident unreachable") && error.contains(expected), "{error}");
                        assert!(access.fallback_runner().is_some(), "a shell runner is insufficient permission");
                    } else {
                        let plan = result.expect("authorized terminal route").plan;
                        let [ResolvedAttachAction::Command(args)] = plan.0.as_slice() else { panic!("single attach hop") };
                        assert!(format!("{plan:?}").contains(expected));
                        assert_eq!(
                            args.contains(&Arg::Literal("--take".into())),
                            matches!(mode, AttachMode::Take | AttachMode::PreferTake)
                        );
                        assert_eq!(args.contains(&Arg::Literal("--strict".into())), mode == AttachMode::Strict);
                        assert_eq!(
                            environments.get("pre-agent").await.unwrap().status,
                            Some(original.clone()),
                            "terminal access leaves health status untouched"
                        );
                    }
                }
            }
            daemon.environment_manager.register_provisioned_environment(handle.id().clone(), handle.clone(), bag, None).unwrap();
            available.store(false, std::sync::atomic::Ordering::SeqCst);
            let error = daemon.attach_resolver().resolve_attach("crew", None, false, AttachMode::Default, None).await.unwrap_err();
            assert!(error.contains("resident vessel agent") && error.contains("provider terminal transport unreachable"), "{error}");
            if let Some(provider) = provider {
                assert_eq!(provider.list().await.unwrap().len(), 1, "terminal failure cannot erase healthy provider inventory");
            } else {
                assert!(
                    daemon.environment_manager.managed_environments().iter().any(|(id, _)| id == handle.id()),
                    "terminal failure retains the independent handle"
                );
            }
        }
    }

    // Transient checkout resolution must retain its selected host without
    // claiming a durable TerminalSession identity, including cross-host hops.
    #[tokio::test]
    async fn transient_checkout_preserves_host_binding() {
        let (daemon, _backend, temp) = attach_fixture().await;
        std::fs::write(temp.path().join("hosts.toml"), "[hosts.kiwi]\nhostname = \"daemon-route\"\nexpected_host_name = \"kiwi\"\n")
            .expect("host routes");
        let resolver = daemon.attach_resolver();
        for host in [daemon.host_name.clone(), HostName::new("kiwi")] {
            let row = CheckoutRow::builder()
                .resource(flotilla_protocol::ResourceRef::new("flotilla.work/v1", "Checkout", "checkout-ns", "checkout"))
                .repo(flotilla_protocol::RepositoryKey("repo".into()))
                .repo_label("repo")
                .path(temp.path().to_string_lossy().into_owned())
                .branch("main")
                .host(host.clone())
                .authority(flotilla_protocol::LifecycleAuthority::Observed)
                .build();
            let target = AttachTarget::Checkout(Box::new(row.clone()));
            assert!(target.resolve(&resolver, &AttachReadContext::default(), &row.path, false, AttachMode::Default).await.is_err());
            for mode in [AttachMode::Default, AttachMode::PreferTake, AttachMode::Strict, AttachMode::Take] {
                let result = target.resolve(&resolver, &AttachReadContext::default(), &row.path, true, mode).await;
                // The fixture's local passthrough pool supports watch/default
                // seats only; remote routing defers seat checks to the owner.
                if host == daemon.host_name && matches!(mode, AttachMode::Strict | AttachMode::Take) {
                    assert_eq!(
                        result.expect_err("unsupported local seat"),
                        "terminal pool does not support controller-seat attach options"
                    );
                    continue;
                }
                let resolved = result.expect("transient checkout");
                let binding = resolved.binding.expect("checkout host binding");
                assert_eq!(binding.host, host);
                assert_eq!(binding.namespace, "checkout-ns");
                assert_eq!(binding.session, None);
                assert_eq!(binding.resource_ref(), None);
            }
        }
    }

    #[tokio::test]
    async fn placed_session_requires_an_unambiguous_cross_origin_convoy_address() {
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        let session = backend
            .using::<ResourceTerminalSession>("flotilla")
            .create(
                &flotilla_resources::InputMeta::builder()
                    .name("placed".to_string())
                    .labels(BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy".to_string())]))
                    .build(),
                &flotilla_resources::TerminalSessionSpec::builder()
                    .env_ref("env".to_string())
                    .role("coder".to_string())
                    .source(flotilla_resources::TerminalSessionSource::Tool { command: "sh".into() })
                    .cwd("/work".to_string())
                    .pool("passthrough".to_string())
                    .build(),
            )
            .await
            .expect("placed session");
        let first = flotilla_protocol::NodeId::new("first");
        let second = flotilla_protocol::NodeId::new("second");
        let placed = flotilla_protocol::NodeId::new("placed");
        let key = |origin| ("flotilla".to_string(), "convoy".to_string(), Some(origin));
        // Origin identity wins. Without an exact origin, placement elsewhere
        // permits a shared address, but different candidate addresses are ambiguous.
        for second_address in ["coder@project", "reviewer@project"] {
            let addresses =
                HashMap::from([(key(first.clone()), "coder@project".to_string()), (key(second.clone()), second_address.to_string())]);
            assert_eq!(session_convoy_address(&session, Some(&first), &addresses).map(String::as_str), Some("coder@project"));
            assert_eq!(
                session_convoy_address(&session, Some(&placed), &addresses).map(String::as_str),
                (second_address == "coder@project").then_some("coder@project")
            );
        }
    }

    // A failed catalog load is stable within one request: later lookups return
    // the same error without running their loader. A new request retries.
    #[tokio::test]
    async fn host_snapshot_caches_load_errors_until_the_next_request() {
        let loads = std::sync::atomic::AtomicUsize::new(0);
        let load = || async {
            loads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err("Host catalog unavailable".to_string())
        };
        let reads = AttachReadContext::default();
        for reference in ["first-host", "second-host"] {
            let error = reads.canonical_host_with_load("flotilla", reference, load()).await.expect_err("load fails");
            assert_eq!(error, "Host catalog unavailable");
        }
        assert_eq!(loads.load(std::sync::atomic::Ordering::Relaxed), 1);
        let fresh = AttachReadContext::default();
        assert_eq!(
            fresh.canonical_host_with_load("flotilla", "first-host", load()).await.expect_err("new request retries"),
            "Host catalog unavailable"
        );
        assert_eq!(loads.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    // A request shares even an empty snapshot across concurrent lookups, isolates
    // namespaces, and releases the snapshot so the next request sees new Hosts.
    #[tokio::test]
    async fn host_snapshot_is_request_scoped_and_namespace_isolated() {
        let (_daemon, backend, _temp) = attach_fixture().await;
        let backend = backend.with_read_counts();
        let reads = AttachReadContext::default();
        let (first, second) =
            tokio::join!(reads.canonical_host(&backend, "empty", "new-host"), reads.canonical_host(&backend, "empty", "new-host"),);
        assert!(first.expect("first lookup").is_none());
        assert!(second.expect("second lookup").is_none());
        backend
            .using::<ResourceHost>("empty")
            .create(&test_meta("new-host"), &HostSpec { display_name: "new".into(), ..Default::default() })
            .await
            .expect("new host");
        assert!(reads.canonical_host(&backend, "empty", "new-host").await.expect("cached absence").is_none());
        assert!(reads.canonical_host(&backend, "other", "new-host").await.expect("other namespace").is_none());
        let fresh = AttachReadContext::default();
        let host = fresh.canonical_host(&backend, "empty", "new-host").await.expect("fresh lookup").expect("new host visible");
        assert_eq!(host.reference.as_str(), "new-host");
        assert_eq!(backend.read_snapshot().calls_by_kind().get("Host"), Some(&6));
    }

    #[test]
    fn recursive_attach_preserves_take_preference_and_explicit_watch() {
        let host = HostName::new("udder");
        let take = flotilla_protocol::arg::flatten(&recursive_attach_command(&host, "crew-session", AttachMode::PreferTake), 0);
        let watch = flotilla_protocol::arg::flatten(&recursive_attach_command(&host, "crew-session", AttachMode::Default), 0);

        assert_eq!(take, "flotilla attach --host 'udder' --transient 'crew-session'");
        assert_eq!(watch, "flotilla attach --host 'udder' --transient --watch 'crew-session'");
    }

    #[test]
    fn remote_fleet_attach_references_use_the_canonical_role_address() {
        let row = FleetListRow::builder()
            .convoy("reviewer @ flotilla")
            .convoy_ref("convoy-opaque")
            .vessel("convoy-opaque-implement")
            .crew("implement/coder")
            .crew_state("running")
            .host(HostName::new("remote"))
            .namespace("flotilla")
            .session("session-opaque")
            .staleness(FleetStaleness::Fresh { last_sync: Utc::now() })
            .build();

        let references = fleet_row_attach_reference_keys(&row);
        assert!(references.contains(&"reviewer@flotilla".to_string()));
        assert!(references.contains(&"reviewer@flotilla/implement/coder".to_string()));
        assert!(references.contains(&"convoy-opaque".to_string()));
        assert!(!references.contains(&"reviewer @ flotilla".to_string()));
        assert_eq!(fleet_row_attach_reference_label(&row), "reviewer @ flotilla/implement/coder (remote)");
    }
    #[tokio::test]
    async fn attach_resolves_role_addresses_to_the_live_record_before_planning_the_hop() {
        let (daemon, backend, _temp) = attach_fixture().await;
        create_identity_convoy(&backend, "convoy-andamento", "governor", Some("andamento")).await;
        create_identity_convoy(&backend, "convoy-flotilla", "governor", Some("flotilla")).await;
        let local_host = daemon.local_host_id().expect("local host identity").to_string();
        backend
            .using::<ResourceHost>("flotilla")
            .create(
                &test_meta(&local_host),
                &HostSpec { display_name: "standing-test".to_string(), connection: Default::default(), ..HostSpec::default() },
            )
            .await
            .expect("local host resource");
        let environment = create_test_environment(&daemon, "governor-env", &local_host).await;
        create_running_session(&daemon, &environment, "governor-session", "convoy-andamento", "governor").await;
        create_running_session(&daemon, &environment, "flotilla-governor-session", "convoy-flotilla", "governor").await;

        let resolver = daemon.attach_resolver();

        let contextual = resolver
            .resolve_attach("governor", None, false, AttachMode::Default, Some("andamento"))
            .await
            .expect("bare role resolves inside project context");
        assert_eq!(contextual.binding.as_ref().and_then(|binding| binding.convoy.as_deref()), Some("convoy-andamento"));
        assert!(matches!(contextual.plan.0.as_slice(), [ResolvedAttachAction::Command(_)]));

        let ambiguous = resolver
            .resolve_attach("governor", None, false, AttachMode::Default, None)
            .await
            .expect_err("bare fleet context must refuse ambiguity");
        assert_eq!(ambiguous, "governor is ambiguous: governor@andamento, governor@flotilla");

        let qualified = resolver
            .resolve_attach("governor@andamento", None, false, AttachMode::Default, None)
            .await
            .expect("qualified role resolves without project context");
        assert_eq!(qualified.binding.as_ref().and_then(|binding| binding.convoy.as_deref()), Some("convoy-andamento"));

        let from_untracked_repo = daemon
            .execute_query(
                Command {
                    node_id: None,
                    provisioning_target: None,
                    context_repo: Some(flotilla_protocol::RepoSelector::Path(PathBuf::from("/scratch/untracked"))),
                    action: CommandAction::Attach { reference: "governor@andamento".to_string(), host: None, mode: AttachMode::Default },
                },
                uuid::Uuid::new_v4(),
            )
            .await
            .expect("untracked cwd context must not abort attach");
        assert!(matches!(from_untracked_repo, CommandValue::AttachCommandResolved { .. }));

        let session_in_project_context = resolver
            .resolve_attach("governor-session", None, false, AttachMode::Default, Some("andamento"))
            .await
            .expect("non-role references must fall back to the attach index in project context");
        assert_eq!(session_in_project_context.binding.as_ref().and_then(|binding| binding.session.as_deref()), Some("governor-session"));

        let wrong_host = resolver
            .resolve_attach("governor@andamento", Some(&HostName::new("udder")), false, AttachMode::Default, None)
            .await
            .expect_err("an explicit host must constrain role-address resolution");
        assert_eq!(wrong_host, "no attach target matching 'governor@andamento' on host 'udder'");
    }

    #[tokio::test]
    async fn failed_convoy_with_live_session_remains_attachable_and_listed() {
        let (daemon, backend, _temp) = attach_fixture().await;
        create_identity_convoy(&backend, "convoy-failed", "coder", Some("flotilla")).await;
        let host = daemon.local_host_id().expect("local host identity").to_string();
        backend
            .using::<ResourceHost>("flotilla")
            .create(
                &test_meta(&host),
                &HostSpec { display_name: "standing-test".to_string(), connection: Default::default(), ..HostSpec::default() },
            )
            .await
            .expect("local host resource");
        let environment = create_test_environment(&daemon, "coder-env", &host).await;
        create_running_session(&daemon, &environment, "coder-session", "convoy-failed", "coder").await;
        let convoys = backend.using::<ResourceConvoy>("flotilla");
        let convoy = convoys.get("convoy-failed").await.expect("convoy");
        convoys
            .update_status(
                "convoy-failed",
                &convoy.metadata.resource_version,
                &ConvoyStatus { phase: ConvoyPhase::Failed, ..Default::default() },
            )
            .await
            .expect("failed convoy status");
        let sessions = backend.using::<ResourceTerminalSession>("flotilla");
        let session = sessions.get("coder-session").await.expect("session");
        let mut status = session.status.expect("session status");
        status.phase = ResourceTerminalSessionPhase::Lost;
        sessions.update_status("coder-session", &session.metadata.resource_version, &status).await.expect("stale session status");

        let resolver = daemon.attach_resolver();
        for reference in ["coder", "coder@flotilla", "convoy-failed", "coder-session"] {
            let resolved = resolver.resolve_attach(reference, None, false, AttachMode::Default, None).await.expect(reference);
            assert_eq!(resolved.binding.as_ref().and_then(|binding| binding.convoy_phase), Some(flotilla_protocol::ConvoyPhase::Failed));
            assert_eq!(serde_json::to_value(&resolved.binding).expect("binding JSON")["convoy_phase"], "failed");
            // Recursive transient hops accept every durable reference kind
            // and retain the viewer's phase/identity information unchanged.
            let transient = resolver.resolve_transient(reference, None).await.expect("transient session resolution");
            assert_eq!(transient.plan, resolved.plan);
            assert_eq!(transient.binding, resolved.binding);
            assert_eq!(resolved.binding.and_then(|binding| binding.session), Some("coder-session".to_string()));
        }
        let rows = daemon.fleet_list_internal().await.expect("fleet list").rows;
        assert!(rows
            .iter()
            .any(|row| row.convoy_ref.as_deref() == Some("convoy-failed") && row.session.as_deref() == Some("coder-session")));
    }

    #[tokio::test]
    async fn bare_role_prefers_newest_connectable_generation() {
        let (daemon, backend, _temp) = attach_fixture().await;
        create_identity_convoy(&backend, "convoy-old", "coder", Some("flotilla")).await;
        let mut successor = ConvoySpec::builder().workflow_ref("review".to_string()).role("coder".to_string()).generation(2).build();
        successor.project_ref = Some("flotilla".to_string());
        backend.using::<ResourceConvoy>("flotilla").create(&test_meta("convoy-new"), &successor).await.expect("successor");
        let host = daemon.local_host_id().expect("local host identity").to_string();
        backend
            .using::<ResourceHost>("flotilla")
            .create(
                &test_meta(&host),
                &HostSpec { display_name: "standing-test".to_string(), connection: Default::default(), ..HostSpec::default() },
            )
            .await
            .expect("local host resource");
        let environment = create_test_environment(&daemon, "coder-env", &host).await;
        create_running_session(&daemon, &environment, "terminal-old", "convoy-old", "coder").await;
        create_running_session(&daemon, &environment, "terminal-new", "convoy-new", "coder").await;

        let resolver = daemon.attach_resolver();
        let resolved = resolver.resolve_attach("coder", None, false, AttachMode::Default, None).await.expect("newest generation");
        assert_eq!(resolved.binding.and_then(|binding| binding.session), Some("terminal-new".to_string()));
        let old = resolver.resolve_attach("convoy-old", None, false, AttachMode::Default, None).await.expect("explicit old generation");
        assert_eq!(old.binding.and_then(|binding| binding.session), Some("terminal-old".to_string()));

        backend.using::<ResourceConvoy>("flotilla").create(&test_meta("convoy-peer"), &successor).await.expect("peer generation");
        create_running_session(&daemon, &environment, "terminal-peer", "convoy-peer", "coder").await;
        let error =
            resolver.resolve_attach("coder", None, false, AttachMode::Default, None).await.expect_err("same generation is ambiguous");
        assert!(error.contains("ambiguous"), "{error}");

        let sessions = backend.using::<ResourceTerminalSession>("flotilla");
        sessions.delete("terminal-peer").await.expect("remove peer endpoint");
        let newest = sessions.get("terminal-new").await.expect("newest session");
        let mut unreachable_spec = newest.spec;
        unreachable_spec.pool = "missing-pool".to_string();
        sessions
            .update(
                &flotilla_resources::InputMeta::builder().name("terminal-new".to_string()).labels(newest.metadata.labels.clone()).build(),
                &newest.metadata.resource_version,
                &unreachable_spec,
            )
            .await
            .expect("make newer endpoint unreachable");
        let fallback = resolver.resolve_attach("coder", None, false, AttachMode::Default, None).await.expect("older reachable generation");
        assert_eq!(fallback.binding.and_then(|binding| binding.session), Some("terminal-old".to_string()));
    }

    // #3042: role/name/prefix attach resolution shares its per-request join,
    // and unrelated convoys/messages cannot increase store reads or decoding.
    #[tokio::test]
    async fn attach_reads_ignore_unrelated_convoys_and_messages() {
        use flotilla_store_testkit::{assert_flat_reads, ReadBudget, ReadOperation, ReadSnapshot};
        async fn attach_reads(reference: &str, unrelated: usize) -> (ReadSnapshot, ReadSnapshot) {
            let (daemon, backend, _temp) = attach_fixture().await;
            create_identity_convoy(&backend, "convoy-main", "coder", Some("flotilla")).await;
            let convoys = backend.using::<ResourceConvoy>("flotilla");
            let convoy = convoys.get("convoy-main").await.expect("convoy");
            convoys
                .update_status(
                    "convoy-main",
                    &convoy.metadata.resource_version,
                    &ConvoyStatus { phase: ConvoyPhase::Failed, ..Default::default() },
                )
                .await
                .expect("convoy phase");
            let host = daemon.local_host_id().expect("host").to_string();
            backend
                .using::<ResourceHost>("flotilla")
                .create(&test_meta(&host), &HostSpec { display_name: "host".into(), connection: Default::default(), ..Default::default() })
                .await
                .expect("host");
            let environment = create_test_environment(&daemon, "env-main", &host).await;
            create_running_session(&daemon, &environment, "terminal-main", "convoy-main", "coder").await;
            for n in 0..unrelated {
                create_identity_convoy(&backend, &format!("unrelated-{n}"), "other", Some("other")).await;
                backend
                    .using::<flotilla_resources::Message>("flotilla")
                    .create(
                        &test_meta(&format!("message-{n}")),
                        &flotilla_resources::MessageSpec::builder()
                            .sender("other/source".into())
                            .receiver("other/receiver".into())
                            .relation(flotilla_resources::MessageRelation::Peer)
                            .body("other".into())
                            .build(),
                    )
                    .await
                    .expect("noise message");
            }
            // Count this resolver's real store collaborators. Daemon startup,
            // projection materialization and provider-retirement tasks share
            // the same data but retain separate, uncounted backend handles.
            let request_backend = backend.clone().with_read_counts();
            let mut resolver = daemon.attach_resolver();
            let request_observations = resolver.observed_resource_backend.clone().with_read_counts();
            resolver.resource_backend = &request_backend;
            resolver.observed_resource_backend = &request_observations;
            // A read through the daemon handle must not leak into the
            // request observer, even when it reads the same resource kind.
            backend.using::<ResourceConvoy>("flotilla").get("convoy-main").await.expect("daemon read");
            assert!(request_backend.read_snapshot().is_empty());
            let resolved = resolver.resolve_attach(reference, None, false, AttachMode::Default, None).await.expect("attach");
            let binding = resolved.binding.expect("binding");
            assert_eq!(binding.session.as_deref(), Some("terminal-main"));
            assert_eq!(binding.convoy_phase, Some(flotilla_protocol::ConvoyPhase::Failed));
            let reads = request_backend.read_snapshot();
            // Direct session references select no Convoy candidates; one
            // exact parent get restores phase without a namespace read.
            let convoy_calls = if reference.starts_with("terminal-") { 2 } else { 1 };
            assert_eq!(reads.calls_by_kind().get("Convoy"), Some(&convoy_calls));
            assert_eq!(reads.objects_by_kind().get("Convoy"), Some(&1));
            assert_eq!(reads.calls_by_kind().get("TerminalSession"), Some(&1));
            // #3078: reachability checks and final planning share one Host
            // catalog snapshot for this request. The store counts its local
            // and replica source lists separately.
            reads.assert_within(
                &ReadBudget::new()
                    .objects("Convoy", ReadOperation::Query, 1, 1)
                    .objects("Convoy", ReadOperation::Get, 1, 1)
                    .objects("TerminalSession", ReadOperation::Query, 1, 1)
                    .objects("TerminalSession", ReadOperation::Get, 1, 1)
                    .objects("Environment", ReadOperation::Get, 2, 2)
                    .calls("Host", ReadOperation::List, 2),
                reference,
            );
            let observed = request_observations.read_snapshot();
            assert_eq!(observed.calls_by_kind().get("TerminalSession"), Some(&1));
            assert_eq!(observed.objects_by_kind().get("TerminalSession"), Some(&0));
            (reads, observed)
        }
        for reference in ["coder", "coder@flotilla", "convoy-main", "terminal-main", "terminal-ma"] {
            assert_flat_reads([0, 40], |unrelated| attach_reads(reference, unrelated)).await;
        }
    }

    // A known convoy without a terminal is still starting, not a deleted session.
    #[tokio::test]
    async fn unstarted_convoy_attach_never_claims_session_disappeared() {
        let (daemon, backend, _temp) = attach_fixture().await;
        create_identity_convoy(&backend, "convoy-starting", "coder", Some("flotilla")).await;
        for reference in ["convoy-starting", "coder", "coder@flotilla"] {
            let error = daemon.attach_resolver().resolve_attach(reference, None, false, AttachMode::Default, None).await.unwrap_err();
            assert!(!error.contains("no longer exists"), "{error}");
            assert!(error.contains("not started yet"), "{error}");
        }
    }

    #[tokio::test]
    async fn deleted_terminal_session_is_named_in_attach_error() {
        let (daemon, backend, _temp) = attach_fixture().await;
        create_identity_convoy(&backend, "convoy-gone", "coder", Some("flotilla")).await;
        let host = daemon.local_host_id().expect("local host identity").to_string();
        backend
            .using::<ResourceHost>("flotilla")
            .create(
                &test_meta(&host),
                &HostSpec { display_name: "standing-test".to_string(), connection: Default::default(), ..HostSpec::default() },
            )
            .await
            .expect("local host resource");
        let environment = create_test_environment(&daemon, "coder-env", &host).await;
        create_running_session(&daemon, &environment, "terminal-gone", "convoy-gone", "coder").await;
        backend.using::<ResourceTerminalSession>("flotilla").delete("terminal-gone").await.expect("delete terminal session");

        let resolver = daemon.attach_resolver();
        let error = resolver.resolve_attach("terminal-gone", None, false, AttachMode::Default, None).await.expect_err("gone session");
        assert_eq!(error, format!("session terminal-gone no longer exists on {}", daemon.host_name));
        let error = resolver.resolve_attach("convoy-gone", None, false, AttachMode::Default, None).await.expect_err("gone convoy session");
        assert_eq!(
            error,
            format!("no session available for convoy-gone on {}; the crew has not started yet or its session has ended", daemon.host_name)
        );
        let error = resolver.resolve_attach("coder@flotilla", None, false, AttachMode::Default, None).await.expect_err("gone role session");
        assert_eq!(
            error,
            format!(
                "no session available for coder@flotilla on {}; the crew has not started yet or its session has ended",
                daemon.host_name
            )
        );
    }
}
