//! Attach target indexing, role resolution, and plan construction.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_protocol::{
    arg::Arg,
    commands::AttachMode,
    qualified_path::HostId,
    result_set::{CheckoutRow, Rows},
    AttachBinding, CanonicalHostId, EnvironmentId, FleetListRow, FleetStaleness, HostName, RepoIdentity, ResolvedAttachAction,
    ResolvedAttachPlan, ResultSet,
};
use flotilla_resources::{
    terminal_session_attach_target, Convoy as ResourceConvoy, Environment as ResourceEnvironment, Project, RepositoryKey, ResourceBackend,
    ResourceProvenance, TerminalSession as ResourceTerminalSession, TerminalSessionPhase as ResourceTerminalSessionPhase, CONVOY_LABEL,
    ROLE_LABEL, VESSEL_LABEL, VESSEL_REF_LABEL,
};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use super::{canonical_placement_host_ref, convoy_address, discover_repo_for_environment, LiveConvoyRecord, RoleAddress};
use crate::{
    aggregator_projection::AggregatorProjectionState,
    config::ConfigStore,
    environment_manager::{EnvironmentManager, ManagedEnvironmentKind},
    event_sink::EventSink,
    fleet::FleetService,
    hop_chain::{
        environment::DockerEnvironmentHopResolver,
        remote::{ssh_resolver_from_config, NoopRemoteHopResolver},
        resolver::HopResolver,
        terminal::NoopTerminalHopResolver,
        Hop, HopPlan, ResolutionContext,
    },
    path_context::ExecutionEnvironmentPath,
    project_declaration::BOOTSTRAP_REPOSITORY_ANNOTATION,
    providers::{discovery::DiscoveryRuntime, registry::ProviderRegistry},
    repo_state::RepoState,
};

/// Read-only fleet replica rows used while building the attach index.
#[async_trait]
pub(super) trait FleetRowsSource: Send + Sync {
    async fn rows(&self) -> Vec<(HostName, Vec<FleetListRow>, Vec<ResultSet>)>;
}

pub(super) struct CachedFleetRows<'a> {
    pub(super) fleet: &'a FleetService,
}

#[async_trait]
impl FleetRowsSource for CachedFleetRows<'_> {
    async fn rows(&self) -> Vec<(HostName, Vec<FleetListRow>, Vec<ResultSet>)> {
        self.fleet.cached_rows_for_configured_hosts().await
    }
}

pub(super) struct AttachResolver<'a> {
    // Wired now so future attach operations can publish through the daemon port.
    pub(super) _event_sink: Arc<dyn EventSink>,
    pub(super) resource_backend: &'a ResourceBackend,
    pub(super) observed_resource_backend: &'a ResourceBackend,
    pub(super) aggregator_projection_state: &'a AggregatorProjectionState,
    pub(super) config: &'a ConfigStore,
    pub(super) host_registry: &'a crate::host_registry::HostRegistry,
    pub(super) environment_manager: &'a EnvironmentManager,
    pub(super) discovery: &'a DiscoveryRuntime,
    pub(super) local_environment_id: &'a EnvironmentId,
    pub(super) host_name: &'a HostName,
    pub(super) namespace: &'a std::sync::RwLock<String>,
    pub(super) fleet_rows: Box<dyn FleetRowsSource + 'a>,
    pub(super) repository_keys_by_path: &'a RwLock<HashMap<PathBuf, RepositoryKey>>,
    pub(super) path_identities: &'a RwLock<HashMap<PathBuf, RepoIdentity>>,
    pub(super) repos: &'a RwLock<HashMap<RepoIdentity, RepoState>>,
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

    async fn resolve_repo_selector(&self, selector: &flotilla_protocol::RepoSelector) -> Result<PathBuf, String> {
        match selector {
            flotilla_protocol::RepoSelector::Path(path) => {
                if self.path_identities.read().await.contains_key(path) {
                    Ok(path.clone())
                } else {
                    Err(format!("repo not tracked: {}", path.display()))
                }
            }
            flotilla_protocol::RepoSelector::Query(query) => {
                let repos = self.repos.read().await;
                let entries: Vec<_> = repos.values().map(|state| (state.preferred_path(), state.slug())).collect();
                crate::resolve::resolve_repo(query, entries.into_iter()).map_err(|e| e.to_string())
            }
            flotilla_protocol::RepoSelector::Identity(identity) => self
                .repos
                .read()
                .await
                .get(identity)
                .map(|state| state.preferred_path().to_path_buf())
                .ok_or_else(|| format!("repo not tracked: {identity}")),
        }
    }

    pub(super) async fn resolve_attach(
        &self,
        reference: &str,
        host: Option<&HostName>,
        transient: bool,
        mode: AttachMode,
        project_context: Option<&str>,
    ) -> Result<ResolvedAttach, String> {
        self.resolve_attach_with_context(reference, host, transient, mode, project_context).await
    }

    pub(super) async fn resolve_transient(&self, reference: &str, host: Option<&HostName>) -> Result<ResolvedAttach, String> {
        if reference.trim().is_empty() {
            return Err("attach reference is required".to_string());
        }
        self.resolve_attach_with_context(reference, host, true, AttachMode::Default, None).await
    }

    pub(super) async fn resolvable_references(&self, references: &[String]) -> Result<HashSet<String>, String> {
        if references.is_empty() {
            return Ok(HashSet::new());
        }
        let index = self.attach_candidate_index().await?;
        let mut resolved = HashSet::new();
        for reference in references {
            if index.resolve(self, reference, None, false, AttachMode::Default).await.is_ok() {
                resolved.insert(reference.clone());
            }
        }
        Ok(resolved)
    }

    pub(super) async fn resolvable_targets(&self, targets: &[(String, HostName)]) -> Result<Vec<bool>, String> {
        let index = self.attach_candidate_index().await?;
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
        let Ok(path) = self.resolve_repo_selector(selector).await else {
            return Ok(None);
        };
        let Some(repository_key) = self.repository_keys_by_path.read().await.get(&path).cloned() else {
            return Ok(None);
        };
        let namespace = self.provisioning_namespace().await;
        let projects = self.resource_backend.definitions::<Project>(&namespace).list().await.map_err(|error| error.to_string())?;
        let mut matches = projects
            .into_iter()
            .filter(|project| project.spec.repositories.iter().any(|repository| repository.repo == repository_key))
            .collect::<Vec<_>>();
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

    async fn resolve_live_convoy_record(&self, reference: &str, project_context: Option<&str>) -> Result<Option<LiveConvoyRecord>, String> {
        let explicit = reference.contains('@');
        let requested = if explicit {
            Some(RoleAddress::from_str(reference)?)
        } else {
            project_context.map(|project| RoleAddress { project: project.to_string(), role: reference.to_string() })
        };
        let namespace = self.provisioning_namespace().await;
        let sources =
            self.resource_backend.including_replicas::<ResourceConvoy>(&namespace).list().await.map_err(|error| error.to_string())?;
        let has_role = sources.items.iter().any(|source| source.object.spec.role == reference);
        if !explicit && requested.is_none() && !has_role {
            return Ok(None);
        }

        let role = requested.as_ref().map_or(reference, |address| address.role.as_str());
        let mut candidates = sources
            .items
            .into_iter()
            .filter(|source| {
                source.object.spec.role == role
                    && source.object.status.as_ref().is_none_or(|status| !status.phase.is_terminal())
                    && requested.as_ref().is_none_or(|address| source.object.spec.project_ref.as_deref() == Some(&address.project))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            (&left.object.spec.project_ref, &left.object.metadata.name).cmp(&(&right.object.spec.project_ref, &right.object.metadata.name))
        });
        match candidates.as_slice() {
            [] if explicit => Err(format!("no live convoy matches `{reference}`")),
            [] => Ok(None),
            [source] => {
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
        if let Some(record) = self.resolve_live_convoy_record(reference, project_context).await? {
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
                    .convoy(record.record_name)
                    .role(record.address.role)
                    .build();
                return Ok(ResolvedAttach { plan, binding: Some(binding) });
            }
            let index = self.attach_candidate_index().await?;
            return index.resolve(self, &record.record_name, host, transient, mode).await;
        }
        let index = self.attach_candidate_index().await?;
        index.resolve(self, reference, host, transient, mode).await
    }

    async fn attach_candidate_index(&self) -> Result<AttachCandidateIndex, String> {
        let namespace = self.provisioning_namespace().await;
        let convoy_addresses = self
            .resource_backend
            .including_replicas::<ResourceConvoy>(&namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?
            .items
            .into_iter()
            .filter(|source| source.object.status.as_ref().is_none_or(|status| !status.phase.is_terminal()))
            .map(|source| {
                let address = convoy_address(&source.object.spec.role, source.object.spec.project_ref.as_deref());
                (source.object.metadata.name, address)
            })
            .collect::<HashMap<_, _>>();
        let durable_sessions = self
            .resource_backend
            .including_replicas::<ResourceTerminalSession>(&namespace)
            .list()
            .await
            .map_err(|err| err.to_string())?
            .items;
        let observed_sessions = self
            .observed_resource_backend
            .clone()
            .using::<ResourceTerminalSession>(&namespace)
            .list()
            .await
            .map_err(|err| err.to_string())?
            .items;
        let mut sessions_by_name = HashMap::new();
        let mut replicated_sessions = Vec::new();
        for session in durable_sessions {
            match session.provenance {
                ResourceProvenance::Local => {
                    sessions_by_name.insert(session.object.metadata.name.clone(), session.object);
                }
                ResourceProvenance::Replica { .. } => replicated_sessions.push(session),
            }
        }
        for session in observed_sessions {
            sessions_by_name.insert(session.metadata.name.clone(), session);
        }
        let mut candidates = Vec::new();
        for session in sessions_by_name.into_values() {
            if session.status.as_ref().map(|status| status.phase) != Some(ResourceTerminalSessionPhase::Running) {
                continue;
            }
            let convoy_address = session.metadata.labels.get(CONVOY_LABEL).and_then(|name| convoy_addresses.get(name));
            candidates.push(AttachCandidate {
                label: attach_reference_label(&session.metadata.name, &session.metadata.labels, convoy_address.map(String::as_str)),
                references: attach_reference_keys(&session.metadata.name, &session.metadata.labels, convoy_address.map(String::as_str)),
                host: self.host_name.clone(),
                target: AttachTarget::Local(Box::new(session)),
            });
        }
        let mut indexed_remote_sessions = HashSet::new();
        for replicated in replicated_sessions {
            let session = replicated.object;
            if session.status.as_ref().map(|status| status.phase) != Some(ResourceTerminalSessionPhase::Running) {
                continue;
            }
            let ResourceProvenance::Replica { origin_root, .. } = replicated.provenance else {
                unreachable!("local durable sessions were partitioned above");
            };
            let Some(host) = self.host_registry.live_routed_host_name(&origin_root).await else {
                continue;
            };
            indexed_remote_sessions.insert((host.clone(), session.metadata.name.clone()));
            let convoy_address = session.metadata.labels.get(CONVOY_LABEL).and_then(|name| convoy_addresses.get(name));
            let convoy = session.metadata.labels.get(CONVOY_LABEL).cloned().unwrap_or_else(|| "-".to_string());
            let role = session.metadata.labels.get(ROLE_LABEL).cloned().unwrap_or_else(|| session.spec.role.clone());
            let crew = session.metadata.labels.get(VESSEL_LABEL).map_or_else(|| role.clone(), |vessel| format!("{vessel}/{role}"));
            let row = FleetListRow::builder()
                .convoy(convoy)
                .vessel(session.spec.env_ref.clone())
                .crew(crew)
                .crew_state("running")
                .host(host.clone())
                .namespace(session.metadata.namespace.clone())
                .session(session.metadata.name.clone())
                .staleness(FleetStaleness::Local)
                .build();
            candidates.push(AttachCandidate {
                label: attach_reference_label(&session.metadata.name, &session.metadata.labels, convoy_address.map(String::as_str)),
                references: attach_reference_keys(&session.metadata.name, &session.metadata.labels, convoy_address.map(String::as_str)),
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

        for (host, rows, result_sets) in self.fleet_rows.rows().await {
            let independent_references = result_sets
                .iter()
                .filter_map(|result_set| result_set.rows.as_independents())
                .flatten()
                .filter_map(|row| row.attach.as_deref())
                .collect::<HashSet<_>>();
            let mut indexed_sessions = HashSet::new();
            for row in &rows {
                if row.crew_state != "running" {
                    continue;
                }
                if let Some(session) = &row.session {
                    if indexed_remote_sessions.contains(&(row.host.clone(), session.clone())) {
                        continue;
                    }
                    if independent_references.contains(session.as_str()) {
                        continue;
                    }
                    indexed_sessions.insert(session.clone());
                }
                candidates.push(AttachCandidate {
                    label: fleet_row_attach_reference_label(row),
                    references: fleet_row_attach_reference_keys(row),
                    host: row.host.clone(),
                    target: AttachTarget::Replica { row: Box::new(row.clone()) },
                });
            }
            for result_set in &result_sets {
                let Rows::Independents { scope: None, rows } = &result_set.rows else { continue };
                for row in rows {
                    let Some(reference) = &row.attach else { continue };
                    if row.phase != flotilla_protocol::SessionPhase::Running || !indexed_sessions.insert(reference.clone()) {
                        continue;
                    }
                    let fleet_row = FleetListRow::builder()
                        .convoy("-")
                        .vessel("-")
                        .crew("-")
                        .crew_state("running")
                        .host(host.clone())
                        .namespace(row.resource.namespace.clone())
                        .session(reference.clone())
                        .staleness(FleetStaleness::Local)
                        .build();
                    candidates.push(AttachCandidate {
                        label: format!("{} ({host})", row.name),
                        references: vec![reference.clone()],
                        host: host.clone(),
                        target: AttachTarget::Replica { row: Box::new(fleet_row) },
                    });
                }
            }
        }
        Ok(AttachCandidateIndex::new(candidates))
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
        let host_ref = environment
            .spec
            .host_direct
            .as_ref()
            .map(|spec| spec.host_ref.as_str())
            .or_else(|| environment.spec.docker.as_ref().map(|spec| spec.host_ref.as_str()))
            .ok_or_else(|| format!("environment {} has no host binding", session.spec.env_ref))?;
        if let Some(destination) =
            self.environment_manager.managed_environments().into_iter().find(|(id, _)| id.as_str() == session.spec.env_ref).and_then(
                |(_, state)| match state {
                    ManagedEnvironmentKind::Direct(direct) => direct.ssh_destination,
                    _ => None,
                },
            )
        {
            let cwd = ExecutionEnvironmentPath::new(&session.spec.cwd);
            let attach_args = self.terminal_pool_attach_args(session, &environment, &cwd, seat).await?;
            let plan = ResolvedAttachPlan::command(vec![
                Arg::Literal("ssh".to_string()),
                Arg::Literal("-tt".to_string()),
                Arg::Literal("-o".to_string()),
                Arg::Literal("BatchMode=yes".to_string()),
                Arg::Quoted(destination),
                Arg::Literal("sh".to_string()),
                Arg::Literal("-lc".to_string()),
                Arg::NestedCommand(attach_args),
            ]);
            // The owning daemon resolves future attach requests; the target
            // has no flotillad to receive a recursive attach command.
            return Ok((plan, self.host_name.clone()));
        }
        let target_host = self.target_host_for_resource_ref(&namespace, host_ref).await?;
        if target_host != *self.host_name {
            let plan = self.recursive_attach_plan_for_remote(&target_host, reference, seat).await?;
            return Ok((plan, target_host));
        }

        let plan = self.local_attach_plan_for_session(session, &environment, seat).await?;
        Ok((plan, self.host_name.clone()))
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

        let resolver = ssh_resolver_from_config(self.config.base_path())?;
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

    async fn local_attach_plan_for_session(
        &self,
        session: &flotilla_resources::ResourceObject<ResourceTerminalSession>,
        environment: &flotilla_resources::ResourceObject<ResourceEnvironment>,
        seat: AttachMode,
    ) -> Result<ResolvedAttachPlan, String> {
        let cwd = ExecutionEnvironmentPath::new(&session.spec.cwd);
        let attach_args = self.terminal_pool_attach_args(session, environment, &cwd, seat).await?;
        if environment.spec.docker.is_some() {
            let environment_id = EnvironmentId::new(session.spec.env_ref.clone());
            let container_name = environment.status.as_ref().and_then(|status| status.docker_container_id.as_deref());
            let container_name =
                container_name.ok_or_else(|| format!("environment {} has no docker container id", session.spec.env_ref))?;
            let environment_resolver =
                DockerEnvironmentHopResolver::new(HashMap::from([(environment_id.clone(), container_name.to_string())]));
            let hop_resolver =
                HopResolver::new(Arc::new(NoopRemoteHopResolver), Arc::new(environment_resolver), Arc::new(NoopTerminalHopResolver));
            let plan = HopPlan(vec![Hop::EnterEnvironment { env_id: environment_id, provider: "docker".to_string() }, Hop::RunCommand {
                command: attach_args,
            }]);
            let mut context = ResolutionContext {
                current_host: self.host_name.clone(),
                current_environment: None,
                working_directory: Some(cwd),
                actions: Vec::new(),
                nesting_depth: 0,
            };
            return hop_resolver.resolve(&plan, &mut context).map(|resolved| ResolvedAttachPlan(resolved.0));
        }
        Ok(ResolvedAttachPlan(vec![ResolvedAttachAction::Command(attach_args)]))
    }

    async fn terminal_pool_attach_args(
        &self,
        session: &flotilla_resources::ResourceObject<ResourceTerminalSession>,
        environment: &flotilla_resources::ResourceObject<ResourceEnvironment>,
        cwd: &ExecutionEnvironmentPath,
        seat: AttachMode,
    ) -> Result<Vec<Arg>, String> {
        let registry = self.registry_for_resource_environment(environment, cwd.as_path()).await?;
        let pool = registry
            .terminal_pools
            .get(&session.spec.pool)
            .map(|(_, pool)| Arc::clone(pool))
            .ok_or_else(|| format!("terminal pool {} unavailable for environment {}", session.spec.pool, session.spec.env_ref))?;
        let attach_target = terminal_session_attach_target(session)?;
        pool.preflight_attach(seat).await?;
        pool.attach_args_for_mode(attach_target.session_id, attach_target.launch_command, cwd, &Vec::new(), seat)
    }

    pub(super) async fn target_host_for_resource_ref(&self, namespace: &str, host_ref: &str) -> Result<HostName, String> {
        let canonical_ref = match canonical_placement_host_ref(self.resource_backend, namespace, host_ref).await {
            Ok(Some(target_host)) => target_host.reference,
            Ok(None) => return Err(format!("references unknown host `{host_ref}`")),
            Err(error) => return Err(error),
        };
        Ok(self.host_name_for_canonical_ref(&canonical_ref))
    }

    async fn registry_for_resource_environment(
        &self,
        environment: &flotilla_resources::ResourceObject<ResourceEnvironment>,
        cwd: &Path,
    ) -> Result<Arc<ProviderRegistry>, String> {
        let environment_id = if let Some(host_direct) = environment.spec.host_direct.as_ref() {
            let canonical_ref =
                match canonical_placement_host_ref(self.resource_backend, &environment.metadata.namespace, &host_direct.host_ref).await {
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

/// An attach resolution: the plan the CLI should execute, plus the
/// structured binding it stamps onto its enclosing PM pane (#708).
#[derive(Debug, Clone)]
pub struct ResolvedAttach {
    pub plan: ResolvedAttachPlan,
    pub binding: Option<AttachBinding>,
}

fn attach_reference_keys(session_name: &str, labels: &BTreeMap<String, String>, convoy_address: Option<&str>) -> Vec<String> {
    let mut refs = vec![session_name.to_string()];

    let convoy = labels.get(CONVOY_LABEL);
    let task = labels.get(VESSEL_LABEL);
    let role = labels.get(ROLE_LABEL);
    let vessel = labels.get(VESSEL_REF_LABEL);

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
        reference: &str,
        transient: bool,
        seat: AttachMode,
    ) -> Result<ResolvedAttach, String> {
        match self {
            Self::Local(session) => {
                let (plan, host) = resolver.attach_plan_for_session(reference, session, seat).await?;
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
                Ok(ResolvedAttach { plan, binding: None })
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
    candidates: Vec<AttachCandidate>,
    exact: HashMap<String, Vec<usize>>,
}

impl AttachCandidateIndex {
    fn new(candidates: Vec<AttachCandidate>) -> Self {
        let mut exact: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, candidate) in candidates.iter().enumerate() {
            for reference in &candidate.references {
                exact.entry(reference.clone()).or_default().push(index);
            }
        }
        Self { candidates, exact }
    }

    async fn resolve(
        &self,
        resolver: &AttachResolver<'_>,
        reference: &str,
        host: Option<&HostName>,
        transient: bool,
        seat: AttachMode,
    ) -> Result<ResolvedAttach, String> {
        if reference.trim().is_empty() {
            return Err("attach reference is required".to_string());
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
                Some(host) => Err(format!("no attach target matching '{reference}' on host '{host}'")),
                None => Err(format!("no attach target matching '{reference}'")),
            },
            [index] => self.candidates[*index].target.resolve(resolver, reference, transient, seat).await,
            _ => {
                let mut labels: Vec<_> = matches.iter().map(|index| self.candidates[*index].label.clone()).collect();
                labels.sort();
                labels.dedup();
                Err(format!("attach reference '{reference}' is ambiguous: {}", labels.join(", ")))
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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use flotilla_protocol::{Command, CommandAction, CommandValue};
    use flotilla_resources::{Host as ResourceHost, HostSpec};

    use super::*;
    use crate::{
        daemon::DaemonHandle,
        in_process::tests::{create_identity_convoy, create_running_session, create_test_environment, standing_ensure_fixture, test_meta},
    };

    struct FakeFleetRows(Vec<(HostName, Vec<FleetListRow>, Vec<ResultSet>)>);

    #[async_trait]
    impl FleetRowsSource for FakeFleetRows {
        async fn rows(&self) -> Vec<(HostName, Vec<FleetListRow>, Vec<ResultSet>)> {
            self.0.clone()
        }
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
        let (daemon, backend, _clock, _temp) = standing_ensure_fixture().await;
        create_identity_convoy(&backend, "convoy-andamento", "governor", Some("andamento")).await;
        create_identity_convoy(&backend, "convoy-flotilla", "governor", Some("flotilla")).await;
        let local_host = daemon.local_host_id().expect("local host identity").to_string();
        backend
            .using::<ResourceHost>("flotilla")
            .create(&test_meta(&local_host), &HostSpec { display_name: "standing-test".to_string(), connection: Default::default() })
            .await
            .expect("local host resource");
        let environment = create_test_environment(&daemon, "governor-env", &local_host).await;
        create_running_session(&daemon, &environment, "governor-session", "convoy-andamento", "governor").await;

        let mut resolver = daemon.attach_resolver();
        resolver.fleet_rows = Box::new(FakeFleetRows(Vec::new()));

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
    async fn candidate_index_reads_fleet_rows_from_its_source() {
        let (daemon, _backend, _clock, _temp) = standing_ensure_fixture().await;
        let host = HostName::new("remote");
        let row = FleetListRow::builder()
            .convoy("reviewer @ flotilla")
            .convoy_ref("convoy-opaque")
            .vessel("vessel-opaque")
            .crew("implement/coder")
            .crew_state("running")
            .host(host.clone())
            .namespace("flotilla")
            .session("remote-session")
            .staleness(FleetStaleness::Local)
            .build();
        let mut resolver = daemon.attach_resolver();
        resolver.fleet_rows = Box::new(FakeFleetRows(vec![(host.clone(), vec![row], Vec::new())]));
        let index = resolver.attach_candidate_index().await.expect("index");
        assert!(index.exact.contains_key("remote-session"));
    }
}
