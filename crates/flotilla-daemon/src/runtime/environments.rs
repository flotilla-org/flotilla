//! Provisioned-environment readoption and durable observation updates.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use flotilla_environment::environment::EnvironmentHandle;
use flotilla_protocol::{CanonicalHostId, EnvironmentId, LifecycleReading};
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provisioning::engine::controller::Backing;
use flotilla_resources::environment_records::EnvironmentRecord;
use tracing::{debug, info, warn};

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

pub(super) fn environment_readiness(environment: &EnvironmentRecord) -> EnvironmentReadiness {
    match &environment.reading {
        LifecycleReading::Ready { identity } => EnvironmentReadiness::Ready { identity: Some(identity.clone()) },
        _ => EnvironmentReadiness::NotReady,
    }
}

pub(super) async fn reconcile_provisioned_environments(state: &Arc<ControllerRuntimeState>, namespace: &str) -> Result<(), String> {
    // This home's environments: the ones its providers realise.
    let candidates = state.live_environments().await.map_err(|error| format!("list environments for adoption: {error}"))?;
    let local_host_id = CanonicalHostId::resolved(&state.local_host_ref);
    let mut environments = Vec::new();
    // Lifecycle read errors abort candidate selection: skipping an unreadable environment
    // could publish the inventory-complete barrier below from incomplete backing evidence.
    // Keep adoption and that teardown evidence pending until every candidate can be judged.
    let (cloud, candidates): (Vec<_>, Vec<_>) = candidates.into_iter().partition(|environment| environment.spec.cloud_sandbox.is_some());
    let mut cloud_jobs = Vec::new();
    for environment in cloud {
        if !environment.spec.provider_address.as_ref().is_some_and(|address| state.environment_providers.get(address).is_ok())
            || !environment_readiness(&environment).is_ready()
        {
            continue;
        }
        let state = Arc::clone(state);
        cloud_jobs.push((environment.name.clone(), async move { reconcile_cloud_environment(&state, &environment).await }));
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
            if let EnvironmentReadiness::Ready { identity } = environment_readiness(&environment) {
                environments.push((environment, identity));
            }
        }
    }
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
            state.local_backing_observed.send_if_modified(|observed| !std::mem::replace(observed, true));
        }
        return collect_cloud_reconciliations(cloud_reconciliations, Vec::new()).await;
    }
    let mut inventories = HashMap::new();
    let mut reconciliations = Vec::new();
    for (environment, container_id) in environments {
        let instance = state.provider_address(&environment.name, &environment.spec, None).await?;
        let receives_host_tools = state.environment_providers.get(&instance)?.receives_host_tools();
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
        let env_id = EnvironmentId::new(environment.name.clone());
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
        reconciliations.push((environment, env_id, container_id, handle, receives_host_tools));
    }
    let results =
        futures::future::join_all(reconciliations.into_iter().map(|(environment, env_id, container_id, handle, receives_host_tools)| {
            let state = Arc::clone(state);
            async move {
                match tokio::time::timeout(
                    ENVIRONMENT_ADOPTION_TIMEOUT,
                    reconcile_provisioned_environment(&state, environment, env_id.clone(), container_id, handle, receives_host_tools),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(format!(
                        "environment {env_id} adoption timed out after {}s; will retry",
                        ENVIRONMENT_ADOPTION_TIMEOUT.as_secs()
                    )),
                }
            }
        }))
        .await;
    // Cloud uncertainty must not suppress the successful Docker inventory
    // barrier used by standing-convoy teardown.
    if results.iter().all(Result::is_ok) {
        state.local_backing_observed.send_if_modified(|observed| !std::mem::replace(observed, true));
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
    environment: EnvironmentRecord,
    env_id: EnvironmentId,
    container_id: Option<String>,
    handle: Option<EnvironmentHandle>,
    receives_host_tools: bool,
) -> Result<(), String> {
    let spec = environment.spec.docker.as_ref().expect("candidate filtering requires a local Docker spec");
    let Some(container_id) = container_id else {
        return mark_unavailable_environment(
            state,
            &env_id,
            BackingDisposition::Failed,
            "ready Docker environment has no container identity",
        )
        .await;
    };
    let Some(handle) = handle else {
        return mark_unavailable_environment(
            state,
            &env_id,
            BackingDisposition::Lost,
            &format!("Docker container {container_id} is not running"),
        )
        .await;
    };
    let observed = handle.observe().await;
    let observation = observed.runtime;
    if let Some(observation) = &observation {
        record_environment_observation(state, &env_id, observation).await?;
    }
    if let Some(termination) = observation.as_ref().and_then(|observation| observation.termination.as_ref()) {
        return mark_unavailable_environment(
            state,
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

    // A container whose agent did not answer adoption keeps its tools until it does.
    // Adoption delivers only what provisioning did: a provider that takes no host
    // tools got neither tools nor a crew socket.
    let agent = handle.agent();
    let agent_ready = agent.ready().await.is_ok();
    if agent_ready && receives_host_tools {
        let context = super::docker::DockerToolContext { state, host_ref: &spec.host_ref, jobs: tokio::sync::OnceCell::new() };
        let tools = state.environment_tools.prepare(crate::environment_tools::DOCKER_PROVIDER_KIND, env_id.as_str(), &context).await?;
        agent.push_tools(&tools).await?;
    }

    // Retry the socket even when the interior registry was already adopted.
    // A bind failure must not become a permanently skipped reconciliation step.
    if let Some(sockets) = state.environment_sockets.as_ref().filter(|_| receives_host_tools) {
        sockets.prepare(env_id.clone()).await?;
    }
    // Restores the forward a restarted flotillad lost, so running crews' HTTP hooks resume.
    if agent_ready {
        if let Err(error) = super::docker::forward_daemon_endpoint(state, &env_id, agent.as_ref()).await {
            warn!(environment = %env_id, %error, "daemon forward unavailable; will retry");
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
                &env_id,
                BackingDisposition::Failed,
                &format!("environment adoption failed: {error}"),
            )
            .await;
        }
        info!(environment = %env_id, container = %container_id, "restored provisioned environment registration");
    }
    state.adopt_provisioned_environment(container_id, handle).await;
    Ok(())
}

/// Persist diagnostics before marking backing lost: dependency watchers may
/// immediately start teardown, and neither inspect nor cgroup evidence survives it.
/// Cloud liveness uses provider facts, including cap loss. Restart adoption
/// re-enters ordinary idempotent provisioning so forwarding and credentials are
/// restored together; a cached image/SSH recipe never proves liveness. A sandbox
/// present but unhealthy is logged, never recorded: only its absence ends the launch.
async fn reconcile_cloud_environment(state: &Arc<ControllerRuntimeState>, environment: &EnvironmentRecord) -> Result<(), String> {
    use flotilla_environment::environment::ObservationState;
    let provider = state.provider_for_environment(&environment.name).await?;
    let id = EnvironmentId::new(&environment.name);
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
                    if let Err(error) = super::provider_path::provision(state, &environment.name, &environment.spec).await {
                        warn!(environment = %environment.name, %error, "cloud sandbox adoption failed; will retry");
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
                if !ready {
                    warn!(environment = %environment.name, message = message.as_deref().unwrap_or_default(), "cloud sandbox is unhealthy");
                }
                Ok(())
            }
            ObservationState::Gone { cause } => {
                mark_unavailable_environment(state, &id, BackingDisposition::Lost, &format!("cloud sandbox gone: {cause:?}")).await
            }
            ObservationState::Stopped { cause } => {
                mark_unavailable_environment(
                    state,
                    &id,
                    BackingDisposition::Lost,
                    &format!("cloud sandbox stopped and resume is unavailable: {cause:?}"),
                )
                .await
            }
            ObservationState::Unknown { reason } => {
                let message = format!("cloud sandbox observation unknown: {reason:?}");
                warn!(environment = %environment.name, %message, "cloud sandbox liveness unknown; will retry");
                Err(message)
            }
        },
        None => {
            mark_unavailable_environment(
                state,
                &id,
                BackingDisposition::Lost,
                "cloud sandbox missing from authoritative provider inventory",
            )
            .await
        }
    }
}

/// Records `observation` of environment `env_id` in this home's memory and, when it changed,
/// reports it as a metric: a runtime observation is the home's own, never replicated (#3108 Q2).
pub(super) async fn record_environment_observation(
    state: &ControllerRuntimeState,
    env_id: &EnvironmentId,
    observation: &flotilla_protocol::EnvironmentRuntimeObservation,
) -> Result<(), String> {
    let reads = state.daemon.lifecycle_reads();
    let environments = reads.environments()?;
    if let Some(observed) = environments.observations().observe(env_id.as_str(), observation) {
        tracing::info!(
            target: "flotilla::metrics",
            environment = %env_id,
            container = observed.container_id.as_deref().unwrap_or_default(),
            memory_usage_bytes = ?observed.memory_usage_bytes,
            terminated = observed.termination.is_some(),
            "environment runtime observed"
        );
    }
    Ok(())
}

pub(super) enum BackingDisposition {
    Lost,
    Failed,
}

/// Asks the provider now whether this home's environment `env_ref` still has its backing, for a
/// crew session about to start in it, or that could not start or disappeared there. One gone is
/// judged lost now, as the next provisioned-environments pass would judge it: its vessel is lost
/// and relaunched, and no session starts or ends in it meanwhile. The reason it is gone; none
/// while it runs, for the host's own and SSH host environments, or where nothing can tell.
pub(super) async fn backing_gone_now(state: &Arc<ControllerRuntimeState>, env_ref: &str) -> Option<String> {
    use flotilla_environment::environment::ObservationState;
    if env_ref == state.host_direct_environment_name || state.ssh_host.contains_key(env_ref) {
        return None;
    }
    let id = EnvironmentId::new(env_ref);
    let cached =
        state.provisioned_environments.lock().await.values().find(|active| active.handle.id() == &id).map(|active| active.handle.clone());
    let handle = match cached {
        Some(handle) => Some(handle),
        None => {
            let provider = state.provider_for_environment(env_ref).await.ok()?;
            provider.inspect(&id).await.ok()?
        }
    };
    let reason = match handle {
        None => format!("environment {env_ref} is gone from its provider's inventory"),
        Some(handle) => {
            let observed = handle.observe().await;
            match (observed.runtime.as_ref().and_then(|runtime| runtime.termination.as_ref()), &observed.state) {
                (Some(termination), _) => format!("environment {env_ref} {termination}"),
                (None, state @ (ObservationState::Stopped { .. } | ObservationState::Gone { .. })) => {
                    format!("environment {env_ref} is not running (observation: {state:?})")
                }
                (None, ObservationState::Running | ObservationState::Unknown { .. }) => return None,
            }
        }
    };
    if let Err(error) = mark_unavailable_environment(state, &id, BackingDisposition::Lost, &reason).await {
        warn!(environment = %id, %error, "a crew session's environment is gone, but its loss is not recorded; the next pass judges it");
    }
    Some(reason)
}

pub(super) async fn mark_unavailable_environment(
    state: &Arc<ControllerRuntimeState>,
    env_id: &EnvironmentId,
    disposition: BackingDisposition,
    message: &str,
) -> Result<(), String> {
    // The pass judged from its live read, made before it listed or probed the backing. Read
    // again now, after that evidence: an environment ended since, or whose vessel has ended so
    // its release is due or running, is its release's to end, and the backing it took down is
    // no loss. Its release starts only once its vessel has ended, so a vessel still live here
    // means the backing was gone before any teardown.
    if !still_wanted(state, env_id).await? {
        debug!(environment = %env_id, cause = message, "backing gone while the environment is released; not judged");
        return Ok(());
    }
    let registered_container = state.daemon.environment_container_name(env_id);
    state.daemon.remove_provisioned_environment(env_id);
    state.provisioned_environments.lock().await.retain(|_, active| active.handle.id() != env_id);
    if let Some(container_id) = registered_container {
        state.provisioned_environments.lock().await.remove(&container_id);
    }
    // The checkouts launched in this environment, at any home, keep its work.
    let reads = state.daemon.lifecycle_reads();
    let environment = env_id.as_str().to_owned();
    let launches =
        tokio::task::spawn_blocking(flotilla_schema::reads::carry(move || reads.checkouts()?.in_environment(&environment, None)))
            .await
            .map_err(|error| error.to_string())??;
    let mut retained_paths: Vec<String> = flotilla_resources::checkout_records::latest_launches(launches)
        .into_values()
        .filter_map(|checkout| checkout.path().map(str::to_owned))
        .collect();
    retained_paths.sort();
    retained_paths.dedup();
    let cause = message;
    let message = match disposition {
        BackingDisposition::Lost => format!("lost, recoverable: {message}; rehydration is not available yet (#2872)"),
        BackingDisposition::Failed => message.to_string(),
    };
    let message = if retained_paths.is_empty() {
        message.clone()
    } else {
        format!("{message}; recover work from retained checkout(s): {}", retained_paths.join(", "))
    };
    // The Environment controller is the launch's single writer: it ends the launch Lost, or
    // Failed with the reason, as the probe observed.
    let observed = match disposition {
        BackingDisposition::Lost => Backing::Absent { reason: Some(cause.to_string()) },
        BackingDisposition::Failed => Backing::Failed { reason: message.clone() },
    };
    state.observe_environment_backing(env_id.as_str(), observed).await;
    warn!(environment = %env_id, %message, "provisioned environment backing is unavailable");
    Ok(())
}

/// Whether this home's environment `env_id` is live and held for no vessel, or for one still
/// live: keyed reads of the environment and its vessel, as the Environment controller judges its
/// holder.
async fn still_wanted(state: &ControllerRuntimeState, env_id: &EnvironmentId) -> Result<bool, String> {
    let reads = state.daemon.lifecycle_reads();
    let name = env_id.as_str().to_owned();
    tokio::task::spawn_blocking(flotilla_schema::reads::carry(move || {
        let Some(environment) = reads.environments()?.get(&name, None)? else { return Ok(false) };
        if environment.ended() {
            return Ok(false);
        }
        match environment.vessel.as_deref() {
            None => Ok(true),
            Some(vessel) => Ok(reads.vessels()?.get(vessel, None)?.is_some_and(|vessel| !vessel.ended())),
        }
    }))
    .await
    .map_err(|error| format!("read environment {env_id} again: {error}"))?
}

// Allow the 60-second SSH/agent connect budget plus asset and credential setup.
pub(super) const ENVIRONMENT_ADOPTION_TIMEOUT: Duration = Duration::from_secs(120);
