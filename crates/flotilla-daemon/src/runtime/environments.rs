//! Provisioned-environment readoption and durable observation updates.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use flotilla_environment::environment::EnvironmentHandle;
use flotilla_protocol::{CanonicalHostId, EnvironmentId, LifecycleReading};
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provisioning::engine::environment::EnvironmentBinding;
use flotilla_resources::lifecycle_reads::ProvisioningKind;
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

pub(super) enum EnvironmentReadiness {
    Ready { identity: Option<String> },
    NotReady,
}

impl EnvironmentReadiness {
    pub(super) fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }
}

pub(super) async fn environment_readiness(
    daemon: &flotilla_core::in_process::InProcessDaemon,
    environment: &ResourceObject<Environment>,
) -> Result<EnvironmentReadiness, String> {
    let reads = daemon.lifecycle_reads();
    let name = environment.metadata.name.clone();
    let reading = tokio::task::spawn_blocking(move || reads.try_reading(ProvisioningKind::Environment, &name, None))
        .await
        .map_err(|error| format!("read Environment lifecycle: {error}"))??;
    Ok(match reading {
        Some(LifecycleReading::Ready { identity }) => EnvironmentReadiness::Ready { identity: Some(identity) },
        Some(_) => EnvironmentReadiness::NotReady,
        None => match environment.status.as_ref() {
            Some(status) if status.phase == EnvironmentPhase::Ready => {
                EnvironmentReadiness::Ready { identity: status.docker_container_id.clone() }
            }
            _ => EnvironmentReadiness::NotReady,
        },
    })
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
    // Lifecycle read errors abort candidate selection: skipping an unreadable environment
    // could publish the inventory-complete barrier below from incomplete backing evidence.
    // Keep adoption and that teardown evidence pending until every candidate can be judged.
    let (cloud, candidates): (Vec<_>, Vec<_>) = candidates.into_iter().partition(|environment| environment.spec.cloud_sandbox.is_some());
    let mut cloud_jobs = Vec::new();
    for environment in cloud {
        if !environment.spec.provider_address.as_ref().is_some_and(|address| state.environment_providers.get(address).is_ok())
            || !environment_readiness(&state.daemon, &environment).await?.is_ready()
        {
            continue;
        }
        let state = Arc::clone(state);
        let namespace = namespace.to_owned();
        cloud_jobs
            .push((environment.metadata.name.clone(), async move { reconcile_cloud_environment(&state, &namespace, &environment).await }));
    }
    // Start cloud work before any Docker inventory/SSH awaits. Dropping this
    // JoinSet aborts its coordinator, which in turn aborts its active jobs.
    let cloud_reconciliations = start_cloud_reconciliations(cloud_jobs);
    for environment in candidates {
        let Some(spec) = environment.spec.docker.as_ref() else {
            continue;
        };
        let Ok(canonical) = canonical_runtime_host_id(&state.daemon, namespace, &local_host_id, &spec.host_ref).await else {
            continue;
        };
        if canonical == local_host_id {
            if let EnvironmentReadiness::Ready { identity } = environment_readiness(&state.daemon, &environment).await? {
                environments.push((environment, identity));
            }
        }
    }
    use flotilla_environment::environment::ENVIRONMENT_PROVIDER_INSTANCE_LABEL;
    // This adoption path still consumes stored Docker status. Its input filter
    // must stay narrower than the profile's per_vessel capability test. Retire
    // the legacy-input bridge and these recovery filters together at #3108.
    if environments.is_empty() {
        let mut observed = false;
        for (_, provider) in state
            .local_registry
            .environment_providers
            .iter()
            .filter(|(_, provider)| provider.legacy_input() == Some(flotilla_environment::environment::EnvironmentInput::Docker))
        {
            provider.list().await?;
            observed = true;
        }
        if observed {
            state.local_backing_observed.store(true, Ordering::Release);
        }
        return collect_cloud_reconciliations(cloud_reconciliations, Vec::new()).await;
    }
    let mut inventories = HashMap::new();
    let mut reconciliations = Vec::new();
    for (environment, container_id) in environments {
        let instance = state
            .provider_address(
                &environment.metadata.name,
                &environment.spec,
                environment.metadata.labels.get(ENVIRONMENT_PROVIDER_INSTANCE_LABEL).map(String::as_str),
            )
            .await?;
        if !inventories.contains_key(&instance) {
            let provider = state.environment_providers.get(&instance)?;
            let handles = provider
                .list()
                .await?
                .into_iter()
                .map(|handle| {
                    let backing = handle.backing_id().to_string();
                    ((handle.id().clone(), backing), handle)
                })
                .collect::<HashMap<_, _>>();
            inventories.insert(instance.clone(), handles);
        }
        let env_id = EnvironmentId::new(environment.metadata.name.clone());
        let handles = inventories.get_mut(&instance).expect("provider inventory loaded");
        // Preceding-generation records stored a Docker transport name. Accept
        // that alias during adoption; new provisioning stores the opaque ID.
        let handle = container_id.as_ref().and_then(|container_id| {
            let key = handles
                .keys()
                .find(|key| key.0 == env_id && (key.1 == *container_id || handles[*key].container_name() == Some(container_id.as_str())))
                .cloned()?;
            handles.remove(&key)
        });
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
    // Cloud uncertainty must not suppress the successful Docker inventory
    // barrier used by standing-convoy teardown.
    if results.iter().all(Result::is_ok) {
        state.local_backing_observed.store(true, Ordering::Release);
    }
    let mut results = results;
    if let Err(error) = collect_cloud_reconciliations(cloud_reconciliations, Vec::new()).await {
        results.push(Err(error));
    }
    let errors = results.into_iter().filter_map(Result::err).collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Each cloud job can hold provider HTTP and SSH preparation work. Sixteen
/// permits ordinary siblings to progress while bounding both kinds of load.
/// FIFO admission in listing order, with immediate refill on any completion,
/// lets fast jobs refill slots without waiting for an entire slow wave.
pub(super) const CLOUD_RECONCILIATION_LIMIT: usize = 16;

/// Admit jobs in FIFO listing order and refill each completed slot.
/// Each deadline starts at admission; the returned set owns the coordinator
/// and its children, so dropping it cancels all outstanding work.
pub(super) fn start_cloud_reconciliations<F>(jobs: Vec<(String, F)>) -> tokio::task::JoinSet<Result<(), String>>
where
    F: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    let mut batch = tokio::task::JoinSet::new();
    batch.spawn(async move {
        let mut queued = jobs.into_iter();
        let mut active = tokio::task::JoinSet::new();
        let mut errors = Vec::new();
        loop {
            while active.len() < CLOUD_RECONCILIATION_LIMIT {
                let Some((name, job)) = queued.next() else {
                    break;
                };
                active.spawn(async move {
                    match tokio::time::timeout(ENVIRONMENT_ADOPTION_TIMEOUT, job).await {
                        Ok(result) => result,
                        // Drop never destroys backing; the next pass label-adopts
                        // it if timeout followed creation but preceded publication.
                        Err(_) => Err(format!("cloud environment {name} adoption timed out; will retry")),
                    }
                });
            }
            match active.join_next().await {
                Some(Ok(Ok(()))) => {}
                Some(Ok(Err(error))) => errors.push(error),
                Some(Err(error)) => errors.push(error.to_string()),
                None => break,
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    });
    batch
}

async fn collect_cloud_reconciliations(mut jobs: tokio::task::JoinSet<Result<(), String>>, mut errors: Vec<String>) -> Result<(), String> {
    while let Some(result) = jobs.join_next().await {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => errors.push(error),
            Err(error) => errors.push(error.to_string()),
        }
    }
    if errors.is_empty() {
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
    let observed = handle.observe().await;
    let observation = observed.runtime;
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
    match observed.state {
        flotilla_environment::environment::ObservationState::Running => {}
        observed_state @ (flotilla_environment::environment::ObservationState::Stopped { .. }
        | flotilla_environment::environment::ObservationState::Gone { .. }) => {
            return mark_unavailable_environment(
                state,
                namespace,
                &env_id,
                BackingDisposition::Lost,
                &format!("Docker container {container_id} is not running (observation: {observed_state:?})"),
            )
            .await;
        }
        flotilla_environment::environment::ObservationState::Unknown { reason } => {
            return Err(format!("Docker container {container_id} liveness check failed for environment {env_id}: {reason:?}; will retry"));
        }
    }

    if let Some(agent) = handle.agent() {
        let context = super::docker::DockerToolContext { state, host_ref: &spec.host_ref, jobs: tokio::sync::OnceCell::new() };
        let tools = state.environment_tools.prepare(crate::environment_tools::DOCKER_PROVIDER_KIND, env_id.as_str(), &context).await?;
        agent.push_tools(&tools).await?;
    }

    // Retry the socket even when the interior registry was already adopted.
    // A bind failure must not become a permanently skipped reconciliation step.
    if let Some(sockets) = &state.environment_sockets {
        sockets.prepare(env_id.clone()).await?;
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
/// Cloud liveness uses provider facts, including cap loss. Restart adoption
/// re-enters ordinary idempotent provisioning so forwarding and credentials are
/// restored together; a cached image/SSH recipe never proves liveness.
async fn reconcile_cloud_environment(
    state: &Arc<ControllerRuntimeState>,
    namespace: &str,
    environment: &ResourceObject<Environment>,
) -> Result<(), String> {
    use flotilla_environment::environment::ObservationState;
    use flotilla_provisioning::reconcilers::EnvironmentRuntime;
    let runtime = super::docker::DockerControllerRuntime { state: state.clone() };
    let provider = runtime.provider_for_environment(&environment.metadata.name).await?;
    let id = EnvironmentId::new(&environment.metadata.name);
    let cached =
        state.provisioned_environments.lock().await.values().find(|active| active.handle.id() == &id).map(|active| active.handle.clone());
    let handle = match cached.clone() {
        Some(handle) => Some(handle),
        None => provider.inspect(&id).await?,
    };
    match handle {
        Some(handle) => match handle.observe().await.state {
            ObservationState::Running => {
                if cached.is_none() {
                    if let Err(error) = runtime.provision_environment(&environment.metadata.name, &environment.spec).await {
                        record_cloud_health(
                            state,
                            namespace,
                            &environment.metadata.name,
                            false,
                            Some(format!("Unknown: adoption failed; will retry: {error}")),
                        )
                        .await?;
                        return Err(error.to_string());
                    }
                }
                let health = handle.detect_readiness().await;
                let ready = matches!(health, flotilla_environment::environment::ProviderReadiness::Ready);
                let message = match health {
                    flotilla_environment::environment::ProviderReadiness::Unknown { reason } => Some(reason),
                    flotilla_environment::environment::ProviderReadiness::MissingTool { reason }
                    | flotilla_environment::environment::ProviderReadiness::MissingCredential { reason } => Some(reason),
                    flotilla_environment::environment::ProviderReadiness::Ready => handle.health_message(),
                };
                record_cloud_health(state, namespace, &environment.metadata.name, ready, message).await?;
                Ok(())
            }
            ObservationState::Gone { cause } => {
                mark_unavailable_environment(state, namespace, &id, BackingDisposition::Lost, &format!("cloud sandbox gone: {cause:?}"))
                    .await
            }
            ObservationState::Stopped { cause } => {
                mark_unavailable_environment(
                    state,
                    namespace,
                    &id,
                    BackingDisposition::Lost,
                    &format!("cloud sandbox stopped and resume is unavailable: {cause:?}"),
                )
                .await
            }
            ObservationState::Unknown { reason } => {
                let message = format!("cloud sandbox observation unknown: {reason:?}");
                record_cloud_health(state, namespace, &environment.metadata.name, false, Some(message.clone())).await?;
                Err(message)
            }
        },
        None => {
            mark_unavailable_environment(
                state,
                namespace,
                &id,
                BackingDisposition::Lost,
                "cloud sandbox missing from authoritative provider inventory",
            )
            .await
        }
    }
}

async fn record_cloud_health(
    state: &ControllerRuntimeState,
    namespace: &str,
    name: &str,
    ready: bool,
    message: Option<String>,
) -> Result<(), String> {
    let environments = state.daemon.resource_backend().using::<Environment>(namespace);
    let current = environments.get(name).await.map_err(|e| e.to_string())?;
    let mut status = current.status.clone().unwrap_or_default();
    if status.message != message || status.ready != ready {
        status.message = message;
        status.ready = ready;
        environments.update_status(name, &current.metadata.resource_version, &status).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(super) async fn record_environment_observation(
    state: &ControllerRuntimeState,
    namespace: &str,
    env_id: &EnvironmentId,
    observation: &flotilla_protocol::EnvironmentRuntimeObservation,
) -> Result<(), String> {
    use flotilla_store::apply_status_patch;
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
    state.provisioned_environments.lock().await.retain(|_, active| active.handle.id() != env_id);
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
    let environments = state.daemon.resource_backend().using::<Environment>(namespace);
    let mut patch = Some(match disposition {
        BackingDisposition::Lost => EnvironmentStatusPatch::MarkLost { message: message.clone() },
        BackingDisposition::Failed => EnvironmentStatusPatch::MarkFailed { message: message.clone() },
    });
    // Behind `[primitives]` the engine decides first (#3629 layer 3): the old status is written
    // only as the engine's answer.
    if let Some(engine) = &state.provisioning {
        match environments.get(env_id.as_str()).await {
            Ok(environment) => {
                // The engine's steps are synchronous SQLite transactions; they run off the async
                // workers, as in `Engine::prepare`.
                let lifecycles = Arc::clone(&engine.environments);
                patch = tokio::task::spawn_blocking(move || {
                    flotilla_provisioning::engine::decide::<EnvironmentBinding>(
                        lifecycles.as_ref(),
                        &environment,
                        patch,
                        chrono::Utc::now(),
                    )
                    .patch
                })
                .await
                .map_err(|error| format!("mark unavailable environment {env_id}: engine decision: {error}"))?;
                if patch.is_none() {
                    // A refused edge (a backing lost before it was realised) or one the engine
                    // already took: the old status stays as it is.
                    tracing::info!(environment = %env_id, "the engine took no edge for this backing; its old status is unchanged");
                }
            }
            Err(ResourceError::NotFound { .. }) => {
                tracing::debug!(environment = %env_id, "environment record gone; nothing to mark");
                patch = None;
            }
            Err(error) => return Err(format!("mark unavailable environment {env_id}: {error}")),
        }
    }
    if let Some(patch) = patch {
        flotilla_store::apply_status_patch(&environments, env_id.as_str(), &patch)
            .await
            .map_err(|error| format!("mark unavailable environment {env_id}: {error}"))?;
    }
    warn!(environment = %env_id, %message, "provisioned environment backing is unavailable");
    Ok(())
}

// Allow the 60-second SSH/agent connect budget plus asset and credential setup.
// Checkpoint streaming runs independently of adoption.
pub(super) const ENVIRONMENT_ADOPTION_TIMEOUT: Duration = Duration::from_secs(120);
