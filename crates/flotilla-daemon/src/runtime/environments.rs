//! Provisioned-environment readoption and durable observation updates.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use flotilla_core::providers::{environment::EnvironmentHandle, ChannelLabel, CommandRunner};
use flotilla_protocol::{CanonicalHostId, EnvironmentId};
use flotilla_resources::{
    Checkout, Environment, EnvironmentPhase, EnvironmentStatusPatch, ResourceError, ResourceObject, Vessel, VesselStatusPatch,
};
use tracing::{info, warn};

use super::docker::{probe_provisioned_environment, verify_declared_agent_adapters};
use super::state::{canonical_runtime_host_id, ControllerRuntimeState};
use crate::environment_tools::RUSTC_LINKER_WRAPPER;

pub(super) async fn stage_remote_rustc_wrapper(runner: &dyn CommandRunner, base: &Path) -> Result<PathBuf, String> {
    let directory = base.join("environment-tools");
    let directory_text = directory.to_str().ok_or("remote wrapper directory is not UTF-8")?;
    runner.run("mkdir", &["-p", directory_text], Path::new("/"), &ChannelLabel::Default).await?;
    runner.run("chmod", &["700", directory_text], Path::new("/"), &ChannelLabel::Default).await?;

    let path = directory.join("rustc-linker-cap");
    let path_text = path.to_str().ok_or("remote wrapper path is not UTF-8")?;
    let current = runner.run("cat", &[path_text], Path::new("/"), &ChannelLabel::Default).await.ok();
    if current.as_deref() == Some(RUSTC_LINKER_WRAPPER)
        && runner.run("test", &["-x", path_text], Path::new("/"), &ChannelLabel::Default).await.is_ok()
    {
        return Ok(path);
    }

    // The runner's write_file is atomic, and the final rename publishes only
    // a complete executable. Concurrent launches may race safely.
    let staged = directory.join(format!(".rustc-linker-cap-{}", uuid::Uuid::new_v4()));
    let staged_text = staged.to_str().ok_or("staged remote wrapper path is not UTF-8")?;
    runner.write_file(&staged, RUSTC_LINKER_WRAPPER).await?;
    runner.run("chmod", &["700", staged_text], Path::new("/"), &ChannelLabel::Default).await?;
    runner.run("mv", &["-f", staged_text, path_text], Path::new("/"), &ChannelLabel::Default).await?;
    Ok(path)
}

pub(super) struct ActiveProvisionedEnvironment {
    pub(super) handle: EnvironmentHandle,
}

pub(super) async fn reconcile_provisioned_environments(state: &Arc<ControllerRuntimeState>, namespace: &str) -> Result<(), String> {
    let candidates = state
        .daemon
        .resource_backend()
        .using::<Environment>(namespace)
        .list()
        .await
        .map_err(|error| format!("list environments for adoption: {error}"))?
        .items;
    let local_host_id = CanonicalHostId::resolved(&state.local_host_ref);
    let mut environments = Vec::new();
    for environment in candidates {
        let Some(spec) = environment.spec.docker.as_ref() else {
            continue;
        };
        let Ok(canonical) = canonical_runtime_host_id(&state.daemon, namespace, &local_host_id, &spec.host_ref).await else {
            continue;
        };
        if canonical == local_host_id && environment.status.as_ref().map(|status| status.phase) == Some(EnvironmentPhase::Ready) {
            environments.push(environment);
        }
    }
    use flotilla_core::providers::environment::{EnvironmentKind, ENVIRONMENT_PROVIDER_INSTANCE_LABEL};
    if environments.is_empty() {
        let mut observed = false;
        for (_, provider) in
            state.local_registry.environment_providers.iter().filter(|(_, provider)| provider.kind() == EnvironmentKind::Docker)
        {
            provider.list().await?;
            observed = true;
        }
        if observed {
            state.local_backing_observed.store(true, Ordering::Release);
        }
        return Ok(());
    }
    let mut inventories = HashMap::new();
    let mut reconciliations = Vec::new();
    for environment in environments {
        let instance = environment.metadata.labels.get(ENVIRONMENT_PROVIDER_INSTANCE_LABEL).cloned();
        if !inventories.contains_key(&instance) {
            let (_, provider) = state
                .local_registry
                .environment_providers
                .select(EnvironmentKind::Docker, instance.as_deref())
                .ok_or("environment provider unavailable or ambiguous during adoption")?;
            let handles = provider
                .list()
                .await?
                .into_iter()
                .filter_map(|handle| {
                    let container = handle.container_name()?.to_string();
                    Some(((handle.id().clone(), container), handle))
                })
                .collect::<HashMap<_, _>>();
            inventories.insert(instance.clone(), handles);
        }
        let env_id = EnvironmentId::new(environment.metadata.name.clone());
        let container_id = environment.status.as_ref().and_then(|status| status.docker_container_id.clone());
        let handles = inventories.get_mut(&instance).expect("provider inventory loaded");
        let handle = container_id.as_ref().and_then(|container_id| handles.remove(&(env_id.clone(), container_id.clone())));
        reconciliations.push((environment, env_id, container_id, handle));
    }
    let results = futures::future::join_all(reconciliations.into_iter().map(|(environment, env_id, container_id, handle)| {
        let state = Arc::clone(state);
        let namespace = namespace.to_string();
        async move {
            match tokio::time::timeout(
                ENVIRONMENT_ADOPTION_TIMEOUT,
                reconcile_provisioned_environment(&state, &namespace, environment, env_id.clone(), container_id, handle),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    Err(format!("environment {env_id} adoption timed out after {}s; will retry", ENVIRONMENT_ADOPTION_TIMEOUT.as_secs()))
                }
            }
        }
    }))
    .await;
    let errors = results.into_iter().filter_map(Result::err).collect::<Vec<_>>();
    if errors.is_empty() {
        state.local_backing_observed.store(true, Ordering::Release);
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

pub(super) async fn reconcile_provisioned_environment(
    state: &Arc<ControllerRuntimeState>,
    namespace: &str,
    environment: ResourceObject<Environment>,
    env_id: EnvironmentId,
    container_id: Option<String>,
    handle: Option<EnvironmentHandle>,
) -> Result<(), String> {
    let spec = environment.spec.docker.as_ref().expect("candidate filtering requires a local Docker spec");
    let Some(container_id) = container_id else {
        return mark_unavailable_environment(
            state,
            namespace,
            &env_id,
            BackingDisposition::Failed,
            "ready Docker environment has no container identity",
        )
        .await;
    };
    let Some(handle) = handle else {
        return mark_unavailable_environment(
            state,
            namespace,
            &env_id,
            BackingDisposition::Lost,
            &format!("Docker container {container_id} is not running"),
        )
        .await;
    };
    let status = handle.status().await;
    let observation = match handle.runtime_observation().await {
        Ok(observation) => observation,
        Err(error)
            if matches!(status, Ok(flotilla_protocol::EnvironmentStatus::Stopped | flotilla_protocol::EnvironmentStatus::Failed(_))) =>
        {
            // Richer evidence is best effort once liveness establishes death.
            // Keep existing samples and mark backing lost rather than retry forever.
            warn!(container = %container_id, %error, "terminal backing observation unavailable");
            None
        }
        Err(error) => return Err(format!("Docker container {container_id} observation failed: {error}; will retry")),
    };
    if let Some(observation) = &observation {
        record_environment_observation(state, namespace, &env_id, observation).await?;
    }
    if let Some(termination) = observation.as_ref().and_then(|observation| observation.termination.as_ref()) {
        return mark_unavailable_environment(
            state,
            namespace,
            &env_id,
            BackingDisposition::Lost,
            &format!("Docker container {container_id} {termination}"),
        )
        .await;
    }
    match status {
        Ok(flotilla_protocol::EnvironmentStatus::Running) => {}
        Ok(status @ (flotilla_protocol::EnvironmentStatus::Stopped | flotilla_protocol::EnvironmentStatus::Failed(_))) => {
            return mark_unavailable_environment(
                state,
                namespace,
                &env_id,
                BackingDisposition::Lost,
                &format!("Docker container {container_id} is not running (status: {status:?})"),
            )
            .await;
        }
        Ok(status) => {
            return Err(format!("Docker container {container_id} for environment {env_id} is not ready (status: {status:?}); will retry"))
        }
        Err(error) => {
            return Err(format!("Docker container {container_id} liveness check failed for environment {env_id}: {error}; will retry"))
        }
    }

    if state.daemon.environment_registry_for_environment(&env_id).is_none() {
        let adoption = async {
            let (bag, registry) = probe_provisioned_environment(state, &env_id, &handle).await?;
            verify_declared_agent_adapters(spec, &registry)?;
            state
                .daemon
                .register_provisioned_environment(env_id.clone(), Arc::clone(&handle), bag, Some(registry))
                .map_err(|error| format!("register adopted environment {env_id}: {error}"))?;
            Ok::<(), String>(())
        }
        .await;
        if let Err(error) = adoption {
            return mark_unavailable_environment(
                state,
                namespace,
                &env_id,
                BackingDisposition::Failed,
                &format!("environment adoption failed: {error}"),
            )
            .await;
        }
        info!(environment = %env_id, container = %container_id, "restored provisioned environment registration");
    }
    state.provisioned_environments.lock().await.entry(container_id).or_insert(ActiveProvisionedEnvironment { handle });
    Ok(())
}

/// Persist diagnostics before marking backing lost: dependency watchers may
/// immediately start teardown, and neither inspect nor cgroup evidence survives it.
pub(super) async fn record_environment_observation(
    state: &ControllerRuntimeState,
    namespace: &str,
    env_id: &EnvironmentId,
    observation: &flotilla_protocol::EnvironmentRuntimeObservation,
) -> Result<(), String> {
    use flotilla_resources::apply_status_patch;
    let backend = state.daemon.resource_backend();
    let environments = backend.using::<Environment>(namespace);
    let environment = environments.get(env_id.as_str()).await.map_err(|error| error.to_string())?;
    if let Some(observation) =
        changed_runtime_observation(environment.status.as_ref().and_then(|status| status.runtime_observation.as_ref()), observation)
    {
        apply_status_patch(&environments, env_id.as_str(), &EnvironmentStatusPatch::ObserveRuntime { observation })
            .await
            .map_err(|error| error.to_string())?;
    }
    let vessels = backend.using::<Vessel>(namespace);
    for vessel in vessels.list().await.map_err(|error| error.to_string())?.items {
        if vessel.status.as_ref().and_then(|status| status.environment_ref.as_deref()) != Some(env_id.as_str()) {
            continue;
        }
        if let Some(observation) =
            changed_runtime_observation(vessel.status.as_ref().and_then(|status| status.runtime_observation.as_ref()), observation)
        {
            apply_status_patch(&vessels, &vessel.metadata.name, &VesselStatusPatch::ObserveRuntime { observation })
                .await
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

/// Keep a sample's original timestamp while its value is unchanged. Compare
/// each dependent independently so newly attached vessels still get their first
/// observation even when the Environment already has the same evidence.
pub(super) fn changed_runtime_observation(
    current: Option<&flotilla_protocol::EnvironmentRuntimeObservation>,
    incoming: &flotilla_protocol::EnvironmentRuntimeObservation,
) -> Option<flotilla_protocol::EnvironmentRuntimeObservation> {
    let mut merged = current.cloned().unwrap_or_default();
    merged.merge(incoming);
    if let Some(current) = current {
        if merged.container_id == current.container_id
            && merged.started_at == current.started_at
            && merged.memory_usage_bytes == current.memory_usage_bytes
        {
            merged.memory_observed_at.clone_from(&current.memory_observed_at);
        }
        if &merged == current {
            return None;
        }
    }
    Some(merged)
}

pub(super) enum BackingDisposition {
    Lost,
    Failed,
}

pub(super) async fn mark_unavailable_environment(
    state: &Arc<ControllerRuntimeState>,
    namespace: &str,
    env_id: &EnvironmentId,
    disposition: BackingDisposition,
    message: &str,
) -> Result<(), String> {
    let registered_container = state.daemon.environment_container_name(env_id);
    state.daemon.remove_provisioned_environment(env_id);
    if let Some(container_id) = registered_container {
        state.provisioned_environments.lock().await.remove(&container_id);
    }
    let mut retained_paths = Vec::new();
    let backend = state.daemon.resource_backend();
    for vessel in backend.using::<Vessel>(namespace).list().await.map_err(|error| error.to_string())?.items {
        let Some(status) = vessel.status.as_ref().filter(|status| status.environment_ref.as_deref() == Some(env_id.as_str())) else {
            continue;
        };
        for checkout in status.checkout_refs.values() {
            match backend.using::<Checkout>(namespace).get(checkout).await {
                Ok(checkout) => {
                    if let Some(path) = checkout.status.as_ref().and_then(|status| status.path.clone()) {
                        retained_paths.push(path);
                    }
                }
                Err(ResourceError::NotFound { .. }) => {}
                Err(error) => return Err(error.to_string()),
            }
        }
    }
    retained_paths.sort();
    retained_paths.dedup();
    let message = match disposition {
        BackingDisposition::Lost => format!("lost, recoverable: {message}; rehydration is not available yet (#2872)"),
        BackingDisposition::Failed => message.to_string(),
    };
    let message = if retained_paths.is_empty() {
        message.clone()
    } else {
        format!("{message}; recover work from retained checkout(s): {}", retained_paths.join(", "))
    };
    flotilla_resources::apply_status_patch(
        &state.daemon.resource_backend().using::<Environment>(namespace),
        env_id.as_str(),
        &match disposition {
            BackingDisposition::Lost => EnvironmentStatusPatch::MarkLost { message: message.clone() },
            BackingDisposition::Failed => EnvironmentStatusPatch::MarkFailed { message: message.clone() },
        },
    )
    .await
    .map_err(|error| format!("mark unavailable environment {env_id}: {error}"))?;
    warn!(environment = %env_id, %message, "provisioned environment backing is unavailable");
    Ok(())
}

pub(super) const ENVIRONMENT_ADOPTION_TIMEOUT: Duration = Duration::from_secs(30);
