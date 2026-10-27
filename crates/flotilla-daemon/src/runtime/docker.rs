//! Docker resource runtime and provisioning cleanup.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_config::ConfigStore;
use flotilla_credentials::{compose_agent_environment, AgentMaterialRegistry, CredentialStore};
use flotilla_discovery::registry::ProviderRegistry;
use flotilla_discovery::run_provisioned_host_detectors;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_environment::environment::{
    contained_daemon_socket_path, CreateOpts, EnvironmentHandle, EnvironmentProvisionError, EnvironmentTool, EnvironmentToolAsset,
    EnvironmentToolAssetAccess, EnvironmentToolAssetKind, EnvironmentVariableUpdate, DAEMON_SOCKET_ENV,
};
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_protocol::{ConfiguredResourceLimits, EnvironmentId};
use flotilla_provider_runtime::execution_context::{AccountVariables, BaseLayer, ObservationContext, LOCALE_VARIABLES};
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provisioning::reconcilers::{EnvironmentProvisioning, EnvironmentRuntime};
use flotilla_resources::{
    Environment, EnvironmentSpec, PlacementPolicy, Resource, ResourceError, Vessel, CREDENTIAL_PERMISSIONS_ENV, CREDENTIAL_REFS_ENV,
    CREDENTIAL_SCOPES_ENV,
};
use tokio::sync::OnceCell;
use tracing::{info, warn};

use super::credentials::stage_agent_environment;
use super::environments::ActiveProvisionedEnvironment;
use super::state::ControllerRuntimeState;
use crate::environment_tools::{EnvironmentToolContext, DOCKER_PROVIDER_KIND};

pub(super) struct DockerControllerRuntime {
    pub(super) state: Arc<ControllerRuntimeState>,
}

impl DockerControllerRuntime {
    pub(super) async fn provision_docker(
        &self,
        name: &str,
        spec: &flotilla_resources::DockerEnvironmentSpec,
    ) -> Result<EnvironmentProvisioning, EnvironmentProvisionError> {
        let result = self.provision_contained(name, spec).await;
        // RetryAdoption keeps the socket paired with the retained backing.
        if matches!(result, Err(EnvironmentProvisionError::Failed(_))) {
            if let Some(sockets) = &self.state.environment_sockets {
                sockets.remove(&EnvironmentId::new(name)).await?;
            }
        }
        result
    }

    // One-generation bridge until #2731. New snapshots and Environments freeze
    // baseline provenance; old records recover it through the live policy.
    pub(super) async fn legacy_baseline_for_environment(&self, name: &str) -> Result<bool, String> {
        let backend = self.state.daemon.resource_backend();
        let environment = match backend.using::<Environment>(&self.state.namespace).get(name).await {
            Ok(environment) => environment,
            Err(ResourceError::NotFound { .. }) => return Ok(false),
            Err(error) => return Err(error.to_string()),
        };
        if environment.metadata.labels.contains_key("flotilla.work/legacy-image-baseline") {
            return Ok(true);
        }
        let Some(owner) =
            environment.metadata.owner_references.iter().find(|owner| owner.controller && owner.kind == Vessel::API_PATHS.kind)
        else {
            return Ok(false);
        };
        let vessel = match backend.using::<Vessel>(&self.state.namespace).get(&owner.name).await {
            Ok(vessel) => vessel,
            Err(ResourceError::NotFound { .. }) => return Ok(false),
            Err(error) => return Err(error.to_string()),
        };
        let policy = match backend.using::<PlacementPolicy>(&self.state.namespace).get(&vessel.spec.placement_policy_ref).await {
            Ok(policy) => policy,
            Err(ResourceError::NotFound { .. }) => return Ok(false),
            Err(error) => return Err(error.to_string()),
        };
        Ok(policy.spec.docker_per_vessel.is_some_and(|docker| docker.legacy_image_baseline_ref().is_some()))
    }

    pub(super) async fn provider_for_environment(
        &self,
        name: &str,
    ) -> Result<Arc<dyn flotilla_environment::environment::EnvironmentProvider>, String> {
        use flotilla_environment::environment::ENVIRONMENT_PROVIDER_INSTANCE_LABEL;
        let environment = self
            .state
            .daemon
            .resource_backend()
            .using::<Environment>(&self.state.namespace)
            .get(name)
            .await
            .map_err(|error| error.to_string())?;
        self.provider_for_spec(
            name,
            &environment.spec,
            environment.metadata.labels.get(ENVIRONMENT_PROVIDER_INSTANCE_LABEL).map(String::as_str),
        )
        .await
    }

    async fn provider_for_spec(
        &self,
        name: &str,
        spec: &EnvironmentSpec,
        instance: Option<&str>,
    ) -> Result<Arc<dyn flotilla_environment::environment::EnvironmentProvider>, String> {
        let address = self.state.provider_address(name, spec, instance).await?;
        self.state.environment_providers.get(&address)
    }
}

impl DockerControllerRuntime {
    /// ADR 0062: a composed image built for registry delivery is pulled as
    /// `repository@digest` with the fleet image cache's pull credential,
    /// admitted by an `image-pull` host-action grant for this host. The crew's
    /// work credentials never carry it. Local builds (an image ID) and other
    /// images use the environment's own credential references.
    pub(super) async fn composed_image_pull_auth(
        &self,
        store: &CredentialStore,
        spec: &flotilla_resources::DockerEnvironmentSpec,
    ) -> Result<Option<flotilla_environment::environment::RegistryAuth>, String> {
        if spec.image_build_ref.is_none() {
            return Ok(None);
        }
        let Some((repository, _)) = spec.image.split_once('@') else { return Ok(None) };
        let fleet = match self
            .state
            .daemon
            .resource_backend()
            .definitions::<flotilla_resources::FleetDesignation>(&self.state.namespace)
            .get(flotilla_resources::FLEET_DESIGNATION_NAME)
            .await
        {
            Ok(fleet) => fleet,
            Err(ResourceError::NotFound { .. }) => return Err(format!("composed image {} needs the fleet image cache", spec.image)),
            Err(error) => return Err(error.to_string()),
        };
        let cache = fleet.spec.image_cache.ok_or_else(|| format!("composed image {} needs the fleet image cache", spec.image))?;
        if repository != cache.repository {
            return Err(format!("composed image {} is not in the fleet image cache {}", spec.image, cache.repository));
        }
        store
            .registry_auth(&self.state.local_host_ref, flotilla_resources::HostImageAction::ImagePull, &cache.pull_credential, repository)
            .await
            .map(Some)
            .map_err(|error| format!("pull credential `{}` for composed image: {error}", cache.pull_credential))
    }
}

pub(super) struct DockerToolContext<'a> {
    pub(super) state: &'a ControllerRuntimeState,
    pub(super) host_ref: &'a str,
    pub(super) jobs: OnceCell<usize>,
}

#[async_trait]
impl EnvironmentToolContext for DockerToolContext<'_> {
    async fn rust_build_jobs(&self) -> Result<usize, String> {
        self.jobs.get_or_try_init(|| self.state.rust_build_jobs(self.host_ref)).await.copied()
    }
}

impl DockerControllerRuntime {
    async fn provision_contained(
        &self,
        name: &str,
        spec: &flotilla_resources::DockerEnvironmentSpec,
    ) -> Result<EnvironmentProvisioning, EnvironmentProvisionError> {
        let legacy_baseline = self.legacy_baseline_for_environment(name).await?;
        let (mut environment_spec, instance) =
            match self.state.daemon.resource_backend().using::<Environment>(&self.state.namespace).get(name).await {
                Ok(environment) => (
                    environment.spec,
                    environment.metadata.labels.get(flotilla_environment::environment::ENVIRONMENT_PROVIDER_INSTANCE_LABEL).cloned(),
                ),
                Err(ResourceError::NotFound { .. }) => {
                    (EnvironmentSpec { cloud_sandbox: None, provider_address: None, host_direct: None, docker: Some(spec.clone()) }, None)
                }
                Err(error) => return Err(error.to_string().into()),
            };
        if environment_spec.cloud_sandbox.is_none() {
            environment_spec.docker = Some(spec.clone());
        }
        let provider = self.provider_for_spec(name, &environment_spec, instance.as_deref()).await?;
        let jobs = environment_spec.cloud_sandbox.as_ref().map(|cloud| cloud.size.cpus as usize);
        let context = DockerToolContext { state: &self.state, host_ref: &spec.host_ref, jobs: OnceCell::new_with(jobs) };
        let mut tools = if provider.receives_host_tools() {
            self.state.environment_tools.prepare(DOCKER_PROVIDER_KIND, name, &context).await?
        } else {
            Vec::new()
        };
        if provider.receives_host_tools() {
            let cloud = environment_spec.cloud_sandbox.is_some();
            let sockets = self
                .state
                .environment_sockets
                .as_ref()
                .ok_or("EnvironmentSocketUnavailable: contained provisioning requires a crew socket server")?;
            let path = sockets.prepare(EnvironmentId::new(name)).await?;
            let environment_socket = contained_daemon_socket_path(path.as_path());
            for tool in &mut tools {
                for update in &mut tool.environment {
                    if let EnvironmentVariableUpdate::Set { name, value, .. } = update {
                        if name == DAEMON_SOCKET_ENV {
                            *value = environment_socket.to_string_lossy().into_owned();
                        }
                    }
                }
                for asset in &mut tool.assets {
                    if !cloud && asset.kind == EnvironmentToolAssetKind::UnixSocket {
                        // Mount a dedicated directory containing only this socket.
                        // Keeping the directory stable permits socket replacement on restart.
                        asset.host_path = flotilla_protocol::DaemonHostPath::new(
                            path.as_path().parent().ok_or("EnvironmentSocketInvalid: socket has no host directory")?,
                        );
                        asset.environment_path = flotilla_protocol::ExecutionEnvironmentPath::new(
                            environment_socket.parent().ok_or("EnvironmentSocketInvalid: socket has no contained directory")?,
                        );
                        asset.kind = EnvironmentToolAssetKind::Directory;
                    }
                }
            }
        }
        // Reject parent traversal rather than guessing how a guest's symlinks
        // resolve it. Path component comparisons already ignore redundant `.`.
        for mount in &spec.mounts {
            if Path::new(&mount.target_path).components().any(|part| part == std::path::Component::ParentDir) {
                return Err(format!("mount target {} must not contain parent traversal", mount.target_path).into());
            }
        }
        for tool in &tools {
            for asset in &tool.assets {
                let reserved_path = match asset.kind {
                    EnvironmentToolAssetKind::UnixSocket => asset
                        .environment_path
                        .as_path()
                        .parent()
                        .ok_or_else(|| format!("Unix socket asset {} has no parent directory", asset.environment_path))?,
                    EnvironmentToolAssetKind::File | EnvironmentToolAssetKind::Directory => asset.environment_path.as_path(),
                };
                if spec.mounts.iter().any(|mount| Path::new(&mount.target_path) == reserved_path) {
                    return Err(format!("mount target {} is reserved for {}", reserved_path.display(), asset.purpose).into());
                }
            }
            for update in &tool.environment {
                if let EnvironmentVariableUpdate::Set { name, purpose, .. } = update {
                    if spec.env.contains_key(name) {
                        return Err(format!("environment variable {name} is reserved for {purpose}").into());
                    }
                }
            }
        }
        let credential_refs = credential_refs_from_environment(spec)?;
        let credential_scopes = credential_scopes_from_environment(spec)?;
        let credential_permissions = credential_permissions_from_environment(spec)?;

        let env_id = EnvironmentId::new(name.to_string());
        let mut environment_variables = spec
            .env
            .iter()
            .filter(|(name, _)| {
                !matches!(
                    name.as_str(),
                    CREDENTIAL_REFS_ENV
                        | CREDENTIAL_SCOPES_ENV
                        | CREDENTIAL_PERMISSIONS_ENV
                        | flotilla_resources::RETIRED_CREW_HOME_KEYS_ENV
                )
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        let credential_config_fragments = match &self.state.credential_store {
            Some(store) => match store.vessel_config_fragments(&credential_refs, &spec.env).await {
                Ok(fragments) => fragments,
                Err(error) => {
                    return Err(discard_uncreated_environment(
                        self.state.credential_store.as_deref(),
                        self.state.agent_material.as_deref(),
                        name,
                        error,
                    )
                    .await)
                }
            },
            None if credential_refs.is_empty() => Vec::new(),
            None => {
                return Err(discard_uncreated_environment(
                    None,
                    self.state.agent_material.as_deref(),
                    name,
                    "host-local credential store unavailable".to_string(),
                )
                .await)
            }
        };
        let agent_material_fragments = self
            .state
            .agent_material
            .as_deref()
            .map(|registry| registry.fragments(&spec.required_agent_adapters, &spec.env))
            .unwrap_or_default();
        let agent_environment_claims =
            match compose_agent_environment(credential_config_fragments.iter().cloned().chain(agent_material_fragments.iter().cloned())) {
                Ok(composed) => composed,
                Err(error) => {
                    return Err(discard_uncreated_environment(
                        self.state.credential_store.as_deref(),
                        self.state.agent_material.as_deref(),
                        name,
                        error,
                    )
                    .await)
                }
            };
        let creation_agent_environment = compose_agent_environment(agent_material_fragments.iter().cloned())
            .expect("agent material fragments already composed successfully with credential claims");
        for (name, value) in &creation_agent_environment.environment {
            if !environment_variables.iter().any(|(existing, _)| existing == name) {
                environment_variables.push((name.clone(), value.clone()));
            }
        }
        let material_deliveries = match &self.state.agent_material {
            Some(registry) => match registry.prepare(name, &spec.required_agent_adapters, &spec.env).await {
                Ok(deliveries) => deliveries,
                Err(error) => {
                    return Err(discard_uncreated_environment(
                        self.state.credential_store.as_deref(),
                        self.state.agent_material.as_deref(),
                        name,
                        error,
                    )
                    .await)
                }
            },
            None => Vec::new(),
        };
        // Component-wise overlap checks rely on the earlier rejection of `..`
        // mount targets. Keep that validation ahead of reserved-path checks.
        for delivery in &material_deliveries {
            let reserved = delivery.mount.environment_path.as_path();
            for mount in &spec.mounts {
                let target = Path::new(&mount.target_path);
                if target.starts_with(reserved) || reserved.starts_with(target) {
                    return Err(
                        format!("mount target {} overlaps reserved agent material at {}", target.display(), reserved.display()).into()
                    );
                }
            }
        }
        let prepared_auth = match &self.state.credential_store {
            Some(store) => match self.composed_image_pull_auth(store, spec).await {
                Ok(Some(auth)) => Some(auth),
                Ok(None) => match store.prepare_registry_pull(name, &credential_refs, &spec.image).await {
                    Ok(auth) => auth,
                    Err(error) => {
                        return Err(discard_uncreated_environment(
                            self.state.credential_store.as_deref(),
                            self.state.agent_material.as_deref(),
                            name,
                            error,
                        )
                        .await)
                    }
                },
                Err(error) => {
                    return Err(discard_uncreated_environment(
                        self.state.credential_store.as_deref(),
                        self.state.agent_material.as_deref(),
                        name,
                        error,
                    )
                    .await)
                }
            },
            None if credential_refs.is_empty() => None,
            // Defence in depth: the earlier credential-config step rejects this
            // state today. Keep registry preflight independently fail-closed if
            // the provisioning steps are reordered in a future change.
            None => {
                return Err(discard_uncreated_environment(
                    None,
                    self.state.agent_material.as_deref(),
                    name,
                    "host-local credential store unavailable".to_string(),
                )
                .await)
            }
        };
        let mut provisioned_mounts = Vec::with_capacity(spec.mounts.len() + material_deliveries.len());
        provisioned_mounts.extend(spec.mounts.iter().map(flotilla_controllers::actuators::provisioned_mount));
        deliver_agent_material(&provider.capabilities(), &material_deliveries, &mut tools, &mut provisioned_mounts).await?;
        let jobs = context.rust_build_jobs().await?;
        let opts = CreateOpts {
            tokens: environment_variables,
            working_directory: None,
            image_pull_policy: spec.pull_policy.into(),
            provisioned_mounts,
            tools,
            prepared_auth,
            cpu_limit: None,
            memory_policy: spec.memory_policy.clone(),
        };
        let prepared = match provider
            .prepare(
                &environment_spec,
                &flotilla_environment::environment::PrepareOpts {
                    legacy_baseline,
                    prepared_auth: opts.prepared_auth.clone(),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                return Err(discard_uncreated_environment(
                    self.state.credential_store.as_deref(),
                    self.state.agent_material.as_deref(),
                    name,
                    error,
                )
                .await)
            }
        };
        let handle = match provider.provision(env_id.clone(), &prepared, opts.into()).await {
            Ok(handle) => handle,
            // Retained backing may still use staged credentials and harness homes.
            Err(error @ EnvironmentProvisionError::RetryAdoption(_)) => return Err(error),
            Err(error) => {
                let cleanup_errors =
                    forget_environment_state(self.state.credential_store.as_deref(), self.state.agent_material.as_deref(), name).await;
                if !cleanup_errors.is_empty() {
                    return Err(format!("{error}; additionally failed to {}", cleanup_errors.join("; ")).into());
                }
                return Err(error);
            }
        };
        let image_ref = handle.image().as_str().to_string();
        drop(agent_environment_claims);
        let local_image_id = match handle.local_image_id() {
            Some(digest) => digest.to_string(),
            None => {
                return Err(discard_failed_environment(
                    &handle,
                    self.state.credential_store.as_deref(),
                    self.state.agent_material.as_deref(),
                    name,
                    format!("docker environment provider did not report an image digest for {name}"),
                )
                .await)
            }
        };

        let container_id = handle.backing_id().to_string();
        // The vessel's work credentials land aboard now; each crew's launch reads its own.
        if let Some(store) = &self.state.credential_store {
            if let Err(error) = store
                .prepare_scoped_with_permissions(name, &credential_refs, &credential_scopes, &credential_permissions, handle.runner())
                .await
            {
                return Err(discard_failed_environment(&handle, Some(store), self.state.agent_material.as_deref(), name, error).await);
            }
        } else if !credential_refs.is_empty() {
            return Err(discard_failed_environment(
                &handle,
                None,
                self.state.agent_material.as_deref(),
                name,
                "host-local credential store unavailable".to_string(),
            )
            .await);
        }
        let resolved_credential_fragments = match &self.state.credential_store {
            Some(store) => match store.vessel_config_fragments_for_runner(&credential_refs, &spec.env, &*handle.runner()).await {
                Ok(fragments) => fragments,
                Err(error) => {
                    return Err(discard_failed_environment(
                        &handle,
                        self.state.credential_store.as_deref(),
                        self.state.agent_material.as_deref(),
                        name,
                        error,
                    )
                    .await)
                }
            },
            None => Vec::new(),
        };
        let resolved_agent_environment =
            compose_agent_environment(resolved_credential_fragments.into_iter().chain(agent_material_fragments.iter().cloned()))
                .expect("resolved fragments preserve the successfully checked agent environment claims");
        if let Err(error) =
            stage_agent_environment(&*handle.runner(), self.state.config.state_dir().as_path(), &resolved_agent_environment.contents).await
        {
            return Err(discard_failed_environment(
                &handle,
                self.state.credential_store.as_deref(),
                self.state.agent_material.as_deref(),
                name,
                error,
            )
            .await);
        }
        for delivery in &material_deliveries {
            let args = delivery.preflight.args.iter().map(String::as_str).collect::<Vec<_>>();
            if let Err(error) = handle.runner().run(&delivery.preflight.command, &args, Path::new("/"), &ChannelLabel::Default).await {
                return Err(discard_failed_environment(
                    &handle,
                    self.state.credential_store.as_deref(),
                    self.state.agent_material.as_deref(),
                    name,
                    format!("{}: {error}", delivery.preflight.failure_context),
                )
                .await);
            }
        }
        let (bag, registry) = match probe_provisioned_environment(&self.state, &env_id, &handle).await {
            Ok(probed) => probed,
            Err(error) => {
                return Err(discard_failed_environment(
                    &handle,
                    self.state.credential_store.as_deref(),
                    self.state.agent_material.as_deref(),
                    name,
                    error,
                )
                .await);
            }
        };
        if let Some(cloud) = &environment_spec.cloud_sandbox {
            if let Err(error) = verify_cloud_image_needs(
                &cloud.probe_needs,
                &cloud.image,
                &handle,
                &bag,
                &self.state.daemon.discovery_runtime().factories.account_variables(),
            )
            .await
            {
                return Err(discard_failed_environment(
                    &handle,
                    self.state.credential_store.as_deref(),
                    self.state.agent_material.as_deref(),
                    name,
                    error,
                )
                .await);
            }
        }
        if let Err(error) = verify_declared_agent_adapters(spec, &registry) {
            return Err(discard_failed_environment(
                &handle,
                self.state.credential_store.as_deref(),
                self.state.agent_material.as_deref(),
                name,
                error,
            )
            .await);
        }
        if let Err(error) = self
            .state
            .daemon
            .register_provisioned_environment(env_id.clone(), Arc::clone(&handle), bag, Some(registry))
            .map_err(|err| format!("failed to register provisioned environment {env_id}: {err}"))
        {
            return Err(discard_failed_environment(
                &handle,
                self.state.credential_store.as_deref(),
                self.state.agent_material.as_deref(),
                name,
                error,
            )
            .await);
        }
        let handle_registry_digest = handle.registry_digest().map(str::to_owned);
        let provider_descriptor = handle.provider_descriptor().map(Box::new);
        let survivors = handle.survivors();
        self.state.provisioned_environments.lock().await.insert(container_id.clone(), ActiveProvisionedEnvironment { handle });
        Ok(EnvironmentProvisioning {
            backing_id: Some(container_id),
            provider_descriptor,
            survivors,
            image_ref: Some(image_ref),
            local_image_id: Some(local_image_id),
            registry_digest: handle_registry_digest,
            configured_limits: Some(ConfiguredResourceLimits {
                cpus: None,
                cpu_shares: Some(flotilla_environment::environment::docker::CREW_CPU_SHARES),
                build_jobs: Some(jobs),
                linker_threads: Some(jobs),
            }),
        })
    }
}

#[async_trait]
impl EnvironmentRuntime for DockerControllerRuntime {
    async fn provision_environment(
        &self,
        name: &str,
        spec: &EnvironmentSpec,
    ) -> Result<flotilla_provisioning::reconcilers::EnvironmentProvisioning, EnvironmentProvisionError> {
        use flotilla_provisioning::reconcilers::EnvironmentProvisioning;
        if let Some(cloud) = &spec.cloud_sandbox {
            let carrier = flotilla_resources::DockerEnvironmentSpec {
                host_ref: self.state.local_host_ref.clone(),
                image: cloud.image.clone(),
                image_build_ref: None,
                declared_agent_adapters: cloud
                    .env
                    .get("FLOTILLA_REQUIRED_AGENT_ADAPTERS")
                    .map(|s| serde_json::from_str(s))
                    .transpose()
                    .map_err(|_| "invalid required adapter declaration")?
                    .unwrap_or_default(),
                required_agent_adapters: cloud
                    .env
                    .get("FLOTILLA_REQUIRED_AGENT_ADAPTERS")
                    .map(|s| serde_json::from_str(s))
                    .transpose()
                    .map_err(|_| "invalid required adapter declaration")?
                    .unwrap_or_default(),
                pull_policy: Default::default(),
                memory_policy: Default::default(),
                mounts: vec![],
                env: cloud.env.clone(),
            };
            return self.provision_docker(name, &carrier).await;
        }
        if let Some(docker) = &spec.docker {
            return self.provision_docker(name, docker).await;
        }
        let provider = self.provider_for_environment(name).await?;
        let prepared = provider.prepare(spec, &Default::default()).await?;
        let handle = provider.provision(EnvironmentId::new(name), &prepared, Default::default()).await?;
        let facts = EnvironmentProvisioning {
            backing_id: Some(handle.backing_id().to_string()),
            local_image_id: handle.local_image_id().map(str::to_owned),
            registry_digest: handle.registry_digest().map(str::to_owned),
            ..Default::default()
        };
        self.state.provisioned_environments.lock().await.insert(handle.backing_id().to_string(), ActiveProvisionedEnvironment { handle });
        Ok(facts)
    }

    async fn destroy(&self, environment_ref: &str, container_id: &str) -> Result<(), String> {
        let active = {
            let environments = self.state.provisioned_environments.lock().await;
            environments
                .get(container_id)
                .or_else(|| environments.values().find(|active| active.handle.id().as_str() == environment_ref))
                .map(|active| active.handle.clone())
        };
        match active {
            Some(handle) => {
                if handle.agent().is_some() {
                    if let Some(registry) = self
                        .state
                        .agent_material
                        .as_ref()
                        .filter(|registry| !registry.codex_home_is_shared(environment_ref, &handle.provisioned_mounts()))
                    {
                        let runner = handle.runner();
                        let capture = tokio::time::timeout(
                            std::time::Duration::from_secs(30),
                            registry.capture_environment_home(environment_ref, runner.as_ref()),
                        )
                        .await;
                        match capture {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => {
                                warn!(environment = environment_ref, %error, "session capture failed; continuing environment teardown")
                            }
                            Err(_) => warn!(environment = environment_ref, "session capture timed out; continuing environment teardown"),
                        }
                    }
                }
                handle.destroy().await?
            }
            None => {
                let provider = self.provider_for_environment(environment_ref).await?;
                provider.destroy(container_id).await?;
            }
        }
        // Recovery addresses Docker by full immutable ID, while adopted handles
        // may still be indexed by container name. Retire all cached handles for
        // the Environment after destruction succeeds. Capture is best effort: a
        // stopped or unreachable agent must never prevent deletion.
        self.state.provisioned_environments.lock().await.retain(|_, active| active.handle.id().as_str() != environment_ref);
        let _ = self.state.daemon.remove_provisioned_environment(&EnvironmentId::new(environment_ref));
        self.cleanup(environment_ref).await
    }

    async fn destroy_unrecorded(&self, environment_ref: &str) -> Result<(), String> {
        let provider = self.provider_for_environment(environment_ref).await?;
        // The record may predate status persistence, or the daemon may have
        // restarted after Docker creation. Mutable mount labels are irrelevant.
        for backing in provider.list_backings().await? {
            if backing.environment_id.as_str() == environment_ref {
                self.destroy(environment_ref, &backing.backing_id).await?;
            }
        }
        self.cleanup(environment_ref).await
    }

    async fn cleanup(&self, environment_ref: &str) -> Result<(), String> {
        // An agentless host's vessel agents go with its Environment (#3792).
        if let (Some(vessels), Some(profile)) = (&self.state.agentless_vessels, self.state.agentless_ssh.get(environment_ref)) {
            vessels.retire_environment(profile).await;
        }
        if let Some(sockets) = &self.state.environment_sockets {
            sockets.remove(&EnvironmentId::new(environment_ref)).await?;
        }
        let resource = self.state.daemon.resource_backend().using::<Environment>(&self.state.namespace).get(environment_ref).await;
        match resource {
            Ok(environment) if environment.spec.host_direct.is_some() => {
                self.provider_for_environment(environment_ref).await?.destroy(environment_ref).await?;
            }
            Ok(_) | Err(ResourceError::NotFound { .. }) => {}
            Err(error) => return Err(error.to_string()),
        }
        let mut components = Path::new(environment_ref).components();
        if !matches!(components.next(), Some(std::path::Component::Normal(_))) || components.next().is_some() {
            // Resource names are not globally constrained to DNS labels. Legacy or
            // malformed identities must not escape the state root or wedge deletion.
            warn!(environment = environment_ref, "skipping environment state cleanup: identity must name one state directory");
            return Ok(());
        }
        let mut cleanup_errors =
            forget_environment_state(self.state.credential_store.as_deref(), self.state.agent_material.as_deref(), environment_ref).await;
        if let Some(registry) = self.state.agent_material.as_deref() {
            let namespace = self.state.daemon.provisioning_namespace().await;
            let backend = self.state.daemon.resource_backend();
            let environment = backend.using::<Environment>(&namespace).get(environment_ref).await;
            // Forced deletion may already have removed the resource owners.
            // Admission names environments env-convoy-<32 hex UUID>-<vessel>.
            let mut convoy_ref = environment_ref
                .strip_prefix("env-")
                .filter(|name| {
                    name.starts_with("convoy-")
                        && name.as_bytes().get(39) == Some(&b'-')
                        && name[7..39].bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                .map(|name| name[..39].to_string())
                .unwrap_or_else(|| "unowned".to_string());
            // A failed owner lookup may archive under this fallback rather than
            // the later-resolved convoy. Preserve the logs and continue cleanup.
            match environment {
                Ok(environment) => {
                    if let Some(owner) = environment.metadata.owner_references.iter().find(|owner| owner.kind == "Vessel") {
                        match backend.using::<Vessel>(&namespace).get(&owner.name).await {
                            Ok(vessel) => convoy_ref = vessel.spec.convoy_ref,
                            Err(ResourceError::NotFound { .. }) => {}
                            Err(error) => cleanup_errors.push(error.to_string()),
                        }
                    }
                }
                Err(ResourceError::NotFound { .. }) => {}
                Err(error) => cleanup_errors.push(error.to_string()),
            }
            if !matches!(Path::new(&convoy_ref).components().collect::<Vec<_>>().as_slice(), [std::path::Component::Normal(_)]) {
                cleanup_errors.push("archive convoy identity must name one directory".to_string());
                convoy_ref = "unowned".into();
            }
            if let Err(error) = registry.archive_environment_home(&convoy_ref, environment_ref).await {
                cleanup_errors.push(error);
            }
        }
        let cleat_state = self.state.config.state_dir().as_path().join("contained-cleat").join(environment_ref);
        match tokio::fs::remove_dir_all(&cleat_state).await {
            Ok(()) => info!(environment = environment_ref, path = %cleat_state.display(), "removed contained-cleat environment state"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => cleanup_errors.push(format!("remove contained-cleat state {}: {error}", cleat_state.display())),
        }
        if !cleanup_errors.is_empty() {
            return Err(cleanup_errors.join("; "));
        }
        Ok(())
    }
}

struct GuestProbeEnvironment<'a>(&'a EnvironmentBag);
impl flotilla_provider_runtime::EnvVars for GuestProbeEnvironment<'_> {
    fn get(&self, key: &str) -> Option<String> {
        self.0.find_env_var(key).map(str::to_owned)
    }
}
pub(super) async fn verify_cloud_image_needs(
    needs: &BTreeSet<flotilla_resources::CapabilityNeed>,
    image: &str,
    handle: &EnvironmentHandle,
    bag: &EnvironmentBag,
    accounts: &AccountVariables,
) -> Result<(), String> {
    if needs.is_empty() {
        return Ok(());
    }
    // The sandbox's agent states its own environment; its probes take that
    // `PATH` and locale as their base layer.
    let declared = std::iter::once("PATH").chain(LOCALE_VARIABLES.iter().copied());
    let base = BaseLayer::declared(
        declared.filter_map(|name| bag.find_env_var(name).map(|value| (name.to_string(), value.to_string()))),
        accounts,
    )?;
    let observation = ObservationContext::new(base, accounts);
    // This helper adopts an already-running command endpoint only for detection;
    // it never provisions or selects a runtime for the sandbox.
    let detector = flotilla_environment::environment::command_provider(
        flotilla_environment::environment::EnvironmentInput::HostDirect,
        handle.runner(),
    );
    let kind = flotilla_resources::FulfilmentKindSpec {
        host_ref: String::new(),
        pool: "cleat".into(),
        cost_class: Default::default(),
        grants: Default::default(),
        realisation: flotilla_resources::FulfilmentRealisation::CloudSandboxPerVessel { image: image.to_owned() },
    };
    let facts = crate::fulfilment_probe::probe_kind(
        &kind,
        crate::fulfilment_probe::DetectionTarget { image: None, provider: detector.as_ref(), context: &observation },
        true,
        &*handle.runner(),
        &GuestProbeEnvironment(bag),
        Path::new("/tmp/flotilla/probe"),
        &mut Default::default(),
    )
    .await?;
    let missing =
        needs.iter().filter(|need| !need.covered_by(&Default::default(), Some(&facts))).map(ToString::to_string).collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("cloud image does not satisfy {}", missing.join(", ")))
    }
}

pub(super) fn verify_declared_agent_adapters(
    spec: &flotilla_resources::DockerEnvironmentSpec,
    registry: &ProviderRegistry,
) -> Result<(), String> {
    let discovered = registry.agent_adapters.ids().collect::<BTreeSet<_>>();
    let missing =
        spec.declared_agent_adapters.iter().map(String::as_str).filter(|adapter| !discovered.contains(adapter)).collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "image `{}` declares agent adapter{} {}, but interior discovery did not find {}",
        spec.image,
        if missing.len() == 1 { "" } else { "s" },
        missing.iter().map(|adapter| format!("`{adapter}`")).collect::<Vec<_>>().join(", "),
        if missing.len() == 1 { "it" } else { "them" },
    ))
}

pub(super) fn credential_refs_from_environment(spec: &flotilla_resources::DockerEnvironmentSpec) -> Result<BTreeSet<String>, String> {
    spec.env
        .get(CREDENTIAL_REFS_ENV)
        .map(|encoded| serde_json::from_str(encoded).map_err(|error| format!("invalid credential references: {error}")))
        .transpose()
        .map(Option::unwrap_or_default)
}

pub(super) fn credential_scopes_from_environment(
    spec: &flotilla_resources::DockerEnvironmentSpec,
) -> Result<BTreeMap<String, BTreeSet<flotilla_resources::RepositoryKey>>, String> {
    spec.env
        .get(CREDENTIAL_SCOPES_ENV)
        .map(|encoded| serde_json::from_str(encoded).map_err(|error| format!("invalid credential scopes: {error}")))
        .transpose()
        .map(Option::unwrap_or_default)
}

pub(super) fn credential_permissions_from_environment(
    spec: &flotilla_resources::DockerEnvironmentSpec,
) -> Result<BTreeMap<String, BTreeMap<String, String>>, String> {
    spec.env
        .get(CREDENTIAL_PERMISSIONS_ENV)
        .map(|encoded| serde_json::from_str(encoded).map_err(|error| format!("invalid credential permissions: {error}")))
        .transpose()
        .map(Option::unwrap_or_default)
}

pub(super) async fn discard_failed_environment(
    handle: &EnvironmentHandle,
    credential_store: Option<&CredentialStore>,
    agent_material: Option<&AgentMaterialRegistry>,
    environment_ref: &str,
    error: String,
) -> EnvironmentProvisionError {
    if !handle.created_for_setup() {
        return EnvironmentProvisionError::RetryAdoption(error);
    }
    let mut cleanup_errors = Vec::new();
    if let Err(cleanup_error) = handle.destroy().await {
        cleanup_errors.push(format!("destroy rejected environment: {cleanup_error}"));
    }
    cleanup_errors.extend(forget_environment_state(credential_store, agent_material, environment_ref).await);
    if cleanup_errors.is_empty() {
        error.into()
    } else {
        format!("{error}; additionally failed to {}", cleanup_errors.join("; ")).into()
    }
}

pub(super) async fn discard_uncreated_environment(
    credential_store: Option<&CredentialStore>,
    agent_material: Option<&AgentMaterialRegistry>,
    environment_ref: &str,
    error: String,
) -> EnvironmentProvisionError {
    let cleanup_errors = forget_environment_state(credential_store, agent_material, environment_ref).await;
    if cleanup_errors.is_empty() {
        error.into()
    } else {
        format!("{error}; additionally failed to {}", cleanup_errors.join("; ")).into()
    }
}

pub(super) async fn forget_environment_state(
    credential_store: Option<&CredentialStore>,
    agent_material: Option<&AgentMaterialRegistry>,
    environment_ref: &str,
) -> Vec<String> {
    let mut cleanup_errors = Vec::new();
    if let Some(store) = credential_store {
        if let Err(cleanup_error) = store.forget_environment(environment_ref).await {
            cleanup_errors.push(format!("remove credential cache: {cleanup_error}"));
        }
    }
    if let Some(registry) = agent_material {
        if let Err(cleanup_error) = registry.discard_delivered_credentials(environment_ref).await {
            cleanup_errors.push(format!("discard delivered agent credential: {cleanup_error}"));
        }
    }
    cleanup_errors
}

pub(super) async fn probe_provisioned_environment(
    state: &ControllerRuntimeState,
    env_id: &EnvironmentId,
    handle: &EnvironmentHandle,
) -> Result<(EnvironmentBag, Arc<ProviderRegistry>), String> {
    let env_vars = handle.env_vars().await?;
    let discovery = state.daemon.discovery_runtime();
    let bag = run_provisioned_host_detectors(&discovery.host_detectors, &*handle.runner(), &env_vars).await;
    let probe_root = ExecutionEnvironmentPath::new(handle.capabilities().workspace_root);
    let config = ConfigStore::with_base(state.config.base_path().as_path().join(format!("env-discovery/{env_id}")));
    let registry = discovery.factories.probe_all(&bag, &config, &probe_root, handle.runner()).await;
    Ok((bag, Arc::new(registry)))
}

// Agent material chooses a transport independently of executable/tool delivery.
pub(super) async fn deliver_agent_material(
    capabilities: &flotilla_protocol::provider::ProviderCapabilities,
    deliveries: &[flotilla_credentials::AgentMaterialDelivery],
    tools: &mut Vec<EnvironmentTool>,
    provisioned_mounts: &mut Vec<flotilla_environment::environment::ProvisionedMount>,
) -> Result<(), String> {
    for delivery in deliveries {
        if !capabilities.material_modes.contains(&flotilla_protocol::provider::MaterialMode::Shared) {
            let metadata =
                tokio::fs::symlink_metadata(delivery.mount.host_path.as_path()).await.map_err(|e| format!("agent material source: {e}"))?;
            let kind = if metadata.is_dir() { EnvironmentToolAssetKind::Directory } else { EnvironmentToolAssetKind::File };
            // Providers without Shared material receive an initial copy after hello.
            // Docker keeps the writable home host-backed for the whole session.
            tools.push(EnvironmentTool::new("agent material", delivery.mount.environment_path.as_path()).with_asset(
                EnvironmentToolAsset::new(
                    delivery.mount.host_path.as_path(),
                    delivery.mount.environment_path.as_path(),
                    kind,
                    EnvironmentToolAssetAccess::ReadOnly,
                    "agent material",
                ),
            ));
        } else {
            provisioned_mounts.push(delivery.mount.clone());
        }
    }
    Ok(())
}
