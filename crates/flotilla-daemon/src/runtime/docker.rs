//! Docker resource runtime and provisioning cleanup.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_controllers::reconcilers::{DockerEnvironmentRuntime, DockerProvisioning};
use flotilla_core::{
    config::ConfigStore,
    discovery_api::EnvironmentBag,
    providers::{
        discovery::run_provisioned_host_detectors,
        environment::{CreateOpts, EnvironmentHandle, EnvironmentToolAssetKind, EnvironmentVariableUpdate, PreparedEnvironmentAuth},
        registry::ProviderRegistry,
        ChannelLabel,
    },
};
use flotilla_credentials::{compose_agent_environment, AgentMaterialRegistry, CredentialStore};
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_protocol::{CanonicalHostId, ConfiguredResourceLimits, EnvironmentId};
use flotilla_resources::{
    Environment, EnvironmentSpec, PlacementPolicy, Resource, ResourceError, Vessel, CREDENTIAL_PERMISSIONS_ENV, CREDENTIAL_REFS_ENV,
    CREDENTIAL_SCOPES_ENV,
};
use tokio::sync::OnceCell;
use tracing::{info, warn};

use super::credentials::{agent_material_environment, stage_agent_environment};
use super::environments::ActiveProvisionedEnvironment;
use super::state::{canonical_runtime_host_id, ControllerRuntimeState};
use crate::environment_tools::{EnvironmentToolContext, DOCKER_PROVIDER_KIND};

pub(super) struct DockerControllerRuntime {
    pub(super) state: Arc<ControllerRuntimeState>,
}

impl DockerControllerRuntime {
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
        kind: flotilla_core::providers::environment::EnvironmentKind,
    ) -> Result<Arc<dyn flotilla_core::providers::environment::EnvironmentProvider>, String> {
        use flotilla_core::providers::environment::ENVIRONMENT_PROVIDER_INSTANCE_LABEL;
        let environment = match self.state.daemon.resource_backend().using::<Environment>(&self.state.namespace).get(name).await {
            Ok(environment) => Some(environment),
            Err(ResourceError::NotFound { .. }) => None,
            Err(error) => return Err(error.to_string()),
        };
        let instance =
            environment.as_ref().and_then(|environment| environment.metadata.labels.get(ENVIRONMENT_PROVIDER_INSTANCE_LABEL)).cloned();
        let registry = if let Some(direct) = environment.as_ref().and_then(|environment| environment.spec.host_direct.as_ref()) {
            let local = CanonicalHostId::resolved(&self.state.local_host_ref);
            let host = canonical_runtime_host_id(&self.state.daemon, &self.state.namespace, &local, &direct.host_ref).await?;
            if host == local {
                Arc::clone(&self.state.local_registry)
            } else {
                let ssh = self.state.agentless_ssh.get(host.as_str()).ok_or("host-direct provider host is not registered")?;
                self.state
                    .daemon
                    .environment_registry_for_environment(&ssh.environment_id)
                    .ok_or("host-direct host registry unavailable")?
            }
        } else {
            Arc::clone(&self.state.local_registry)
        };
        registry
            .environment_providers
            .select(kind, instance.as_deref())
            .map(|(_, provider)| Arc::clone(provider))
            .ok_or_else(|| format!("environment provider unavailable or ambiguous for {name} (instance {instance:?}, kind {kind:?})"))
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

#[async_trait]
impl DockerEnvironmentRuntime for DockerControllerRuntime {
    async fn provision_environment(
        &self,
        name: &str,
        spec: &EnvironmentSpec,
    ) -> Result<flotilla_controllers::reconcilers::environment::EnvironmentProvisioning, String> {
        use flotilla_controllers::reconcilers::environment::EnvironmentProvisioning;
        use flotilla_core::providers::environment::EnvironmentKind;
        let kind = EnvironmentKind::of(spec)?;
        if kind == EnvironmentKind::Docker {
            return self.provision(name, spec.docker.as_ref().expect("kind checked")).await.map(EnvironmentProvisioning::Docker);
        }
        let provider = self.provider_for_environment(name, kind).await?;
        let prepared = provider.prepare(spec, &Default::default()).await?;
        provider.provision(EnvironmentId::new(name), &prepared, Default::default()).await?;
        Ok(EnvironmentProvisioning::HostDirect)
    }

    async fn ensure_image(
        &self,
        build: &flotilla_resources::ResourceObject<flotilla_resources::ImageBuild>,
        host: &str,
    ) -> Result<Option<flotilla_resources::PlacedImageIdentity>, String> {
        if host != self.state.local_host_ref {
            return Err("image distribution target is not the local daemon host".into());
        }
        let distributor = self.state.image_distributor.as_ref().ok_or("image distributor unavailable")?;
        distributor.request(build).await
    }

    async fn provision(&self, name: &str, spec: &flotilla_resources::DockerEnvironmentSpec) -> Result<DockerProvisioning, String> {
        let legacy_baseline = self.legacy_baseline_for_environment(name).await?;
        let context = DockerToolContext { state: &self.state, host_ref: &spec.host_ref, jobs: OnceCell::new() };
        let tools = self.state.environment_tools.prepare(DOCKER_PROVIDER_KIND, name, &context).await?;
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
                    return Err(format!("mount target {} is reserved for {}", reserved_path.display(), asset.purpose));
                }
            }
            for update in &tool.environment {
                if let EnvironmentVariableUpdate::Set { name, purpose, .. } = update {
                    if spec.env.contains_key(name) {
                        return Err(format!("environment variable {name} is reserved for {purpose}"));
                    }
                }
            }
        }
        let credential_refs = credential_refs_from_environment(spec)?;
        let credential_scopes = credential_scopes_from_environment(spec)?;
        let credential_permissions = credential_permissions_from_environment(spec)?;
        let provider = self.provider_for_environment(name, flotilla_core::providers::environment::EnvironmentKind::Docker).await?;

        let env_id = EnvironmentId::new(name.to_string());
        let mut environment_variables = spec
            .env
            .iter()
            .filter(|(name, _)| !matches!(name.as_str(), CREDENTIAL_REFS_ENV | CREDENTIAL_SCOPES_ENV | CREDENTIAL_PERMISSIONS_ENV))
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
        let prepared_auth = match &self.state.credential_store {
            Some(store) => match store.prepare_registry_pull(name, &credential_refs, &spec.image).await {
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
            None if credential_refs.is_empty() => PreparedEnvironmentAuth::NoRegistryCredential,
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
        for delivery in &material_deliveries {
            provisioned_mounts.push(delivery.mount.clone());
        }
        let jobs = context.rust_build_jobs().await?;
        let opts = CreateOpts {
            tokens: environment_variables,
            working_directory: None,
            image_pull_policy: spec.pull_policy.into(),
            provisioned_mounts,
            tools,
            prepared_auth,
            cpu_limit: Some(jobs),
            memory_policy: spec.memory_policy.clone(),
        };
        let prepared = match provider
            .prepare(
                &EnvironmentSpec { host_direct: None, docker: Some(spec.clone()) },
                &flotilla_core::providers::environment::PrepareOpts { legacy_baseline, prepared_auth: opts.prepared_auth.clone() },
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
            Err(error) => {
                let cleanup_errors =
                    forget_environment_state(self.state.credential_store.as_deref(), self.state.agent_material.as_deref(), name).await;
                if !cleanup_errors.is_empty() {
                    return Err(format!("{error}; additionally failed to {}", cleanup_errors.join("; ")));
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

        let container_id = handle.container_name().map(ToString::to_string).unwrap_or_else(|| format!("flotilla-env-{}", env_id));
        let delivered_credential_environment = if let Some(store) = &self.state.credential_store {
            match store
                .prepare_scoped_with_permissions(name, &credential_refs, &credential_scopes, &credential_permissions, handle.runner())
                .await
            {
                Ok(environment) => environment,
                Err(error) => {
                    return Err(discard_failed_environment(&handle, Some(store), self.state.agent_material.as_deref(), name, error).await)
                }
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
        } else {
            Vec::new()
        };
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
        if let Some(agent_material) = &self.state.agent_material {
            let mut environment = match agent_material_environment(
                agent_material,
                &spec.required_agent_adapters,
                &spec.env,
                resolved_agent_environment.environment.iter().chain(&delivered_credential_environment).cloned(),
            ) {
                Ok(environment) => environment,
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
            };
            // ADR 0047: remove forwarding of the legacy selection key one roll
            // after #2673, once stored environments are rewritten or reaped.
            for key in ["FLOTILLA_CREW_SKILLS", "FLOTILLA_RESOLVED_SKILLS"] {
                if let Some(selection) = spec.env.get(key) {
                    environment.push((key.into(), selection.clone()));
                }
            }
            let mut source_token_files = BTreeMap::new();
            let will_stage_skills =
                match agent_material.will_stage_skills(&spec.required_agent_adapters, &environment, &*handle.runner()).await {
                    Ok(will_stage) => will_stage,
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
                };
            if will_stage_skills {
                let requests = match agent_material.selected_skill_source_credentials(&environment).await {
                    Ok(requests) => requests,
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
                };
                let mut prepared_by_credential: BTreeMap<String, (String, PathBuf)> = BTreeMap::new();
                for request in requests {
                    let token_file = if let Some((repository, token_file)) = prepared_by_credential.get(&request.credential) {
                        if repository != &request.repository {
                            tracing::warn!(environment = name, source = %request.source, revision = %request.revision, credential = %request.credential, error = "one credential cannot be narrowed to multiple source repositories", "skill-source credential mint failed");
                            let error = format!(
                                "skill source {} credential {} mint failed: one credential cannot be narrowed to multiple source repositories",
                                request.source, request.credential
                            );
                            return Err(discard_failed_environment(
                                &handle,
                                self.state.credential_store.as_deref(),
                                self.state.agent_material.as_deref(),
                                name,
                                error,
                            )
                            .await);
                        }
                        token_file.clone()
                    } else {
                        let Some(store) = &self.state.credential_store else {
                            tracing::warn!(environment = name, source = %request.source, revision = %request.revision, credential = %request.credential, error = "host-local credential store unavailable", "skill-source credential mint failed");
                            let error = format!(
                                "skill source {} credential {} mint failed: host-local credential store unavailable",
                                request.source, request.credential
                            );
                            return Err(discard_failed_environment(&handle, None, self.state.agent_material.as_deref(), name, error).await);
                        };
                        match store.prepare_skill_source(&request.credential, &request.repository, &*handle.runner()).await {
                            Ok(token_file) => {
                                prepared_by_credential.insert(request.credential.clone(), (request.repository.clone(), token_file.clone()));
                                token_file
                            }
                            Err(error) => {
                                tracing::warn!(environment = name, source = %request.source, revision = %request.revision, credential = %request.credential, %error, "skill-source credential mint failed");
                                let error =
                                    format!("skill source {} credential {} mint failed: {error}", request.source, request.credential);
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
                    };
                    source_token_files.insert(request.source, token_file);
                }
            }
            if let Err(error) =
                agent_material.stage_skills(name, &spec.required_agent_adapters, &environment, &source_token_files, &*handle.runner()).await
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
        self.state.provisioned_environments.lock().await.insert(container_id.clone(), ActiveProvisionedEnvironment { handle });
        Ok(DockerProvisioning {
            container_id,
            image_ref,
            local_image_id,
            registry_digest: handle_registry_digest,
            configured_limits: Some(ConfiguredResourceLimits { cpus: Some(jobs), build_jobs: Some(jobs), linker_threads: Some(jobs) }),
        })
    }

    async fn destroy(&self, environment_ref: &str, container_id: &str) -> Result<(), String> {
        let active = self.state.provisioned_environments.lock().await.remove(container_id);
        match active {
            Some(active) => active.handle.destroy().await?,
            None => {
                let provider =
                    self.provider_for_environment(environment_ref, flotilla_core::providers::environment::EnvironmentKind::Docker).await?;
                provider.destroy(container_id).await?;
            }
        }
        // Recovery addresses Docker by full immutable ID, while adopted handles
        // may still be indexed by container name. Remove the requested map key above,
        // then retire any other cached handle whose Environment ID matches.
        self.state.provisioned_environments.lock().await.retain(|_, active| active.handle.id().as_str() != environment_ref);
        let _ = self.state.daemon.remove_provisioned_environment(&EnvironmentId::new(environment_ref));
        self.cleanup(environment_ref).await
    }

    async fn destroy_unrecorded(&self, environment_ref: &str) -> Result<(), String> {
        let provider =
            self.provider_for_environment(environment_ref, flotilla_core::providers::environment::EnvironmentKind::Docker).await?;
        // The record may predate status persistence, or the daemon may have
        // restarted after Docker creation. Mutable mount labels are irrelevant.
        for backing in provider.list_backings().await? {
            if backing.environment_id.as_str() == environment_ref {
                self.destroy(environment_ref, &backing.container_id).await?;
            }
        }
        self.cleanup(environment_ref).await
    }

    async fn cleanup(&self, environment_ref: &str) -> Result<(), String> {
        let resource = self.state.daemon.resource_backend().using::<Environment>(&self.state.namespace).get(environment_ref).await;
        match resource {
            Ok(environment) if environment.spec.host_direct.is_some() => {
                self.provider_for_environment(environment_ref, flotilla_core::providers::environment::EnvironmentKind::HostDirect)
                    .await?
                    .destroy(environment_ref)
                    .await?;
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
) -> String {
    let mut cleanup_errors = Vec::new();
    if let Err(cleanup_error) = handle.destroy().await {
        cleanup_errors.push(format!("destroy rejected environment: {cleanup_error}"));
    }
    cleanup_errors.extend(forget_environment_state(credential_store, agent_material, environment_ref).await);
    if cleanup_errors.is_empty() {
        error
    } else {
        format!("{error}; additionally failed to {}", cleanup_errors.join("; "))
    }
}

pub(super) async fn discard_uncreated_environment(
    credential_store: Option<&CredentialStore>,
    agent_material: Option<&AgentMaterialRegistry>,
    environment_ref: &str,
    error: String,
) -> String {
    let cleanup_errors = forget_environment_state(credential_store, agent_material, environment_ref).await;
    if cleanup_errors.is_empty() {
        error
    } else {
        format!("{error}; additionally failed to {}", cleanup_errors.join("; "))
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
    let probe_root = ExecutionEnvironmentPath::new("/workspace");
    let config = ConfigStore::with_base(state.config.base_path().as_path().join(format!("env-discovery/{env_id}")));
    let registry = discovery.factories.probe_all(&bag, &config, &probe_root, handle.runner()).await;
    Ok((bag, Arc::new(registry)))
}
