//! Local and agentless-SSH discovery and fulfilment observations.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
    time::Duration,
};

use chrono::Utc;
use flotilla_core::{
    config::ConfigStore,
    discovery_api::EnvironmentBag,
    in_process::InProcessDaemon,
    providers::{discovery::EnvVars, registry::ProviderRegistry, ChannelLabel, CommandRunner},
};
use flotilla_credentials::CredentialStore;
use flotilla_paths::path_context::DaemonHostPath;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_protocol::EnvironmentId;
use flotilla_resources::{
    host_direct_environment_name, ConditionValue, FulfilmentFacts, FulfilmentKind, FulfilmentRealisation, Host, HostCondition,
    HostConnection, HostSpec, HostStatus, InputMeta, ModelProbeState, Platform, ResourceBackend, ResourceError, AGENTLESS_CAPABILITY,
    AGENT_ADAPTERS_CAPABILITY, CREDENTIAL_EXPIRY_CAPABILITY, HELD_CREDENTIALS_CAPABILITY, OWNING_DAEMON_CAPABILITY, PLACEMENT_CAPABILITY,
    TRANSPORT_CAPABILITY,
};
use serde_json::json;
use tokio::sync::RwLock;
use tracing::warn;

use super::seed::{
    empty_meta, ensure_default_policies, ensure_host_direct_environment_exists, kind_belongs_to_host, migrate_live_placement_policies,
};
use super::tasks::FULFILMENT_CHANGE_CHECK_INTERVAL;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LocalProvisioningProfile {
    pub(super) host_id: String,
    pub(super) display_name: String,
    pub(super) repo_default_dir: String,
    pub(super) host_direct_pool: String,
    pub(super) docker_pool: String,
    pub(super) available_pools: Vec<String>,
    pub(super) available_agent_adapters: BTreeSet<String>,
    pub(super) docker_available: bool,
}

#[derive(Clone)]
pub(super) struct AgentlessSshProfile {
    pub(super) provisioning: LocalProvisioningProfile,
    pub(super) environment_id: EnvironmentId,
    pub(super) destination: String,
    pub(super) env_bag: EnvironmentBag,
    pub(super) runner: Arc<dyn CommandRunner>,
    pub(super) fulfilment_facts: Arc<RwLock<BTreeMap<String, FulfilmentFacts>>>,
}

pub(super) struct BagEnvVars<'a>(pub(super) &'a EnvironmentBag);

impl EnvVars for BagEnvVars<'_> {
    fn get(&self, key: &str) -> Option<String> {
        self.0.find_env_var(key).map(ToString::to_string)
    }
}

pub(super) async fn discover_agentless_ssh_profiles(daemon: &Arc<InProcessDaemon>, config: &ConfigStore) -> Vec<AgentlessSshProfile> {
    let mut profiles = Vec::new();
    for (environment_id, direct) in daemon.agentless_ssh_environments() {
        let profile = discover_agentless_ssh_profile(daemon, config, environment_id, direct).await;
        match profile {
            Ok(profile) => profiles.push(profile),
            Err(error) => warn!(%error, "failed to discover agentless SSH host; continuing startup"),
        }
    }
    profiles
}

pub(super) async fn discover_agentless_ssh_profile(
    daemon: &Arc<InProcessDaemon>,
    config: &ConfigStore,
    environment_id: EnvironmentId,
    direct: flotilla_core::environment_manager::DirectEnvironmentState,
) -> Result<AgentlessSshProfile, String> {
    let host_id = direct.host_id.as_ref().expect("agentless SSH environment has a host ID").to_string();
    let home = direct.env_bag.find_env_var("HOME").ok_or_else(|| format!("SSH host {host_id} has no HOME"))?;
    let probe_root = ExecutionEnvironmentPath::new(home);
    let remote_state_root =
        direct.env_bag.find_env_var("XDG_STATE_HOME").map(str::to_string).unwrap_or_else(|| format!("{home}/.local/state"));
    let remote_config = ConfigStore::new(config.base_path().clone(), DaemonHostPath::new(format!("{remote_state_root}/flotilla")));
    let registry = Arc::new(
        daemon.discovery_runtime().factories.probe_all(&direct.env_bag, &remote_config, &probe_root, Arc::clone(&direct.runner)).await,
    );
    let pool = ["cleat"]
        .into_iter()
        .find(|name| registry.terminal_pools.contains_key(name))
        .ok_or_else(|| format!("SSH host {host_id} has no persistent terminal pool (cleat)"))?
        .to_string();
    let available_pools = registry.terminal_pools.iter().map(|(description, _)| description.implementation.clone()).collect();
    let provisioning = LocalProvisioningProfile {
        host_id,
        display_name: direct.display_name.unwrap_or_else(|| environment_id.to_string()),
        repo_default_dir: format!("{home}/{DEFAULT_REPO_DIR_SUFFIX}"),
        host_direct_pool: pool,
        docker_pool: "cleat".to_string(),
        available_pools,
        available_agent_adapters: registry.agent_adapters.ids().map(ToString::to_string).collect(),
        docker_available: false,
    };
    daemon.set_direct_environment_registry(&environment_id, Arc::clone(&registry))?;
    let destination = direct.ssh_destination.ok_or_else(|| format!("SSH host {} has no destination", provisioning.host_id))?;
    Ok(AgentlessSshProfile {
        provisioning,
        environment_id,
        destination,
        env_bag: direct.env_bag,
        runner: direct.runner,
        fulfilment_facts: Arc::new(RwLock::new(BTreeMap::new())),
    })
}

pub(super) async fn register_agentless_ssh_resources(
    backend: &ResourceBackend,
    namespace: &str,
    owner_host_id: &str,
    profile: &AgentlessSshProfile,
) -> Result<(), String> {
    let provisioning = &profile.provisioning;
    if provisioning.host_id == owner_host_id {
        return Err(format!("agentless SSH host {} shares the owning daemon's Host identity", provisioning.host_id));
    }
    let hosts = backend.clone().using::<Host>(namespace);
    let mut spec = HostSpec {
        display_name: provisioning.display_name.clone(),
        connection: HostConnection::AgentlessSsh { owning_daemon: owner_host_id.to_string(), destination: profile.destination.clone() },
        ..HostSpec::default()
    };
    match hosts.get(&provisioning.host_id).await {
        Ok(existing) => {
            spec.expected_concurrent_rust_crews = existing.spec.expected_concurrent_rust_crews;
            if existing.spec != spec {
                hosts
                    .update(&InputMeta::from(&existing.metadata), &existing.metadata.resource_version, &spec)
                    .await
                    .map_err(|error| error.to_string())?;
            }
        }
        Err(ResourceError::NotFound { .. }) => {
            hosts.create(&empty_meta(&provisioning.host_id), &spec).await.map_err(|error| error.to_string())?;
        }
        Err(error) => return Err(error.to_string()),
    }
    ensure_host_direct_environment_exists(backend, namespace, provisioning).await?;
    ensure_default_policies(backend, namespace, provisioning).await?;
    let platform = agentless_platform(profile.runner.as_ref()).await;
    migrate_live_placement_policies(backend, namespace, &provisioning.host_id, &platform).await
}

pub(super) async fn agentless_platform(runner: &dyn CommandRunner) -> String {
    let uname = tokio::time::timeout(Duration::from_secs(15), runner.run("uname", &["-s"], Path::new("/"), &ChannelLabel::Default)).await;
    match uname {
        Ok(Ok(output)) => match output.trim() {
            "Darwin" => Platform::Macos.to_string(),
            "Linux" => Platform::Linux.to_string(),
            other => other.to_ascii_lowercase(),
        },
        _ => match tokio::time::timeout(Duration::from_secs(15), runner.run("cmd", &["/c", "ver"], Path::new("/"), &ChannelLabel::Default))
            .await
        {
            Ok(Ok(output)) if output.contains("Windows") => Platform::Windows.to_string(),
            _ => "unknown".to_string(),
        },
    }
}

pub(super) async fn apply_agentless_ssh_observation(
    daemon: &Arc<InProcessDaemon>,
    namespace: &str,
    ssh: &AgentlessSshProfile,
    credential_store: Option<&CredentialStore>,
) -> Result<(), String> {
    let profile = &ssh.provisioning;
    let hosts = daemon.resource_backend().using::<Host>(namespace);
    let host = hosts.get(&profile.host_id).await.map_err(|error| error.to_string())?;
    let probe = ssh.runner.run("mkdir", &["-p", &profile.repo_default_dir], Path::new("/"), &ChannelLabel::Default).await;
    let probe_succeeded = probe.is_ok();
    let free_bytes = if probe_succeeded {
        let output = ssh.runner.run("df", &["-Pk", &profile.repo_default_dir], Path::new("/"), &ChannelLabel::Default).await;
        output
            .ok()
            .and_then(|output| output.lines().last()?.split_whitespace().nth(3)?.parse::<u64>().ok())
            .and_then(|kib| kib.checked_mul(1024))
    } else {
        None
    };
    let ready = probe_succeeded && free_bytes.is_some();
    // Declared credentials are resolved by the owning daemon and delivered
    // through this SSH runner. Ambient login instead belongs to the remote
    // GUI user and must be observed on that host.
    let held_credentials = match credential_store {
        Some(store) => store.held_credentials().await?,
        None => BTreeSet::new(),
    };
    let mut credential_expiry = BTreeMap::new();
    if let Some(expiry) = CredentialStore::remote_ambient_claude_expiry(&ssh.env_bag, ssh.runner.as_ref()).await {
        credential_expiry.insert(flotilla_resources::AMBIENT_CLAUDE_CREDENTIAL_SCOPE.to_string(), expiry);
    }
    let mut conditions = probe
        .err()
        .map(|error| {
            vec![HostCondition::builder()
                .condition_type("Transport/SSH")
                .value(ConditionValue::False)
                .reason("Unreachable")
                .message(error)
                .observed_at(Utc::now())
                .build()]
        })
        .unwrap_or_default();
    if probe_succeeded && free_bytes.is_none() {
        conditions.push(
            HostCondition::builder()
                .condition_type("Capacity/FreeSpace")
                .value(ConditionValue::False)
                .reason("MeasurementUnavailable")
                .message(format!("could not measure free space at {} over SSH", profile.repo_default_dir))
                .observed_at(Utc::now())
                .build(),
        );
    }
    if probe_succeeded {
        let platform = agentless_platform(ssh.runner.as_ref()).await;
        migrate_live_placement_policies(&daemon.resource_backend(), namespace, &profile.host_id, &platform).await?;
    }
    let fulfilment_facts = if probe_succeeded {
        let observed = ssh.fulfilment_facts.read().await.clone();
        if observed.is_empty() {
            host.status.as_ref().map(|status| status.fulfilment_facts.clone()).unwrap_or_default()
        } else {
            observed
        }
    } else {
        BTreeMap::new()
    };
    let status = HostStatus {
        capabilities: BTreeMap::from([
            (AGENT_ADAPTERS_CAPABILITY.to_string(), json!(profile.available_agent_adapters)),
            (HELD_CREDENTIALS_CAPABILITY.to_string(), json!(held_credentials)),
            (CREDENTIAL_EXPIRY_CAPABILITY.to_string(), json!(credential_expiry)),
            ("terminal_pools".to_string(), json!(profile.available_pools)),
            (AGENTLESS_CAPABILITY.to_string(), json!(true)),
            (TRANSPORT_CAPABILITY.to_string(), json!("ssh")),
            (PLACEMENT_CAPABILITY.to_string(), json!("host_direct_only")),
            (OWNING_DAEMON_CAPABILITY.to_string(), json!(daemon.local_host_id().map(|id| id.to_string()))),
        ]),
        fulfilment_facts,
        model_probes: host.status.as_ref().map(|status| status.model_probes.clone()).unwrap_or_default(),
        // This timestamp is the owning daemon's last successful SSH probe,
        // never a heartbeat emitted by a daemon on the target host.
        heartbeat_at: probe_succeeded.then(Utc::now),
        ready,
        sleeping_until: host.status.as_ref().and_then(|status| status.sleeping_until),
        disk_free_bytes: free_bytes,
        admission_free_space_floor_bytes: Some(daemon.admission_free_space_floor_bytes()?),
        conditions,
        ..HostStatus::default()
    };
    hosts.update_status(&profile.host_id, &host.metadata.resource_version, &status).await.map_err(|error| error.to_string())?;
    Ok(())
}

impl LocalProvisioningProfile {
    pub(super) fn host_direct_environment_name(&self) -> String {
        host_direct_environment_name(&self.host_id)
    }

    pub(super) fn host_direct_policy_name(&self) -> String {
        host_direct_environment_name(&self.host_id)
    }

    pub(super) fn docker_policy_name(&self) -> String {
        format!("docker-on-{}", self.host_id)
    }
}

pub(super) async fn probe_local_provider_registry(
    daemon: &Arc<InProcessDaemon>,
    config: &ConfigStore,
) -> Result<Arc<ProviderRegistry>, String> {
    let local_bag = daemon.local_environment_bag().ok_or_else(|| "local environment bag unavailable".to_string())?;
    let runner = daemon.local_command_runner().ok_or_else(|| "local command runner unavailable".to_string())?;
    let probe_root = daemon
        .tracked_repo_paths()
        .await
        .into_iter()
        .next()
        .map(ExecutionEnvironmentPath::new)
        .unwrap_or_else(|| ExecutionEnvironmentPath::new("/"));
    Ok(Arc::new(daemon.discovery_runtime().factories.probe_all(&local_bag, config, &probe_root, runner).await))
}

pub(super) fn build_local_profile(
    daemon: &Arc<InProcessDaemon>,
    local_registry: &ProviderRegistry,
) -> Result<LocalProvisioningProfile, String> {
    let host_id = daemon.local_host_id().ok_or_else(|| "local host id unavailable".to_string())?.to_string();
    let repo_default_dir = daemon
        .local_environment_bag()
        .and_then(|bag| bag.find_env_var("HOME").map(|home| format!("{home}/{DEFAULT_REPO_DIR_SUFFIX}")))
        .or_else(|| daemon.discovery_runtime().env.get("HOME").map(|home| format!("{home}/{DEFAULT_REPO_DIR_SUFFIX}")))
        .unwrap_or_else(|| "/tmp/flotilla-repos".to_string());

    let mut available_pools: Vec<_> = local_registry.terminal_pools.iter().map(|(desc, _)| desc.implementation.clone()).collect();
    available_pools.sort();
    available_pools.dedup();

    let host_direct_pool = local_registry.terminal_pools.preferred_name().unwrap_or("passthrough").to_string();
    let docker_pool = "cleat".to_string();
    let docker_available =
        local_registry.environment_providers.for_kind(flotilla_core::providers::environment::EnvironmentKind::Docker).is_some()
            && local_registry.terminal_pools.contains_key(&docker_pool);
    let available_agent_adapters = local_registry.agent_adapters.ids().map(ToString::to_string).collect();

    Ok(LocalProvisioningProfile {
        host_id,
        display_name: daemon.host_name().to_string(),
        repo_default_dir,
        host_direct_pool,
        docker_pool,
        available_pools,
        available_agent_adapters,
        docker_available,
    })
}

pub(super) struct FulfilmentProbeContext<'a> {
    pub(super) runner: &'a dyn CommandRunner,
    pub(super) env: &'a dyn EnvVars,
    pub(super) scratch: &'a Path,
}

pub(super) async fn observe_fulfilment_facts(
    backend: &ResourceBackend,
    namespace: &str,
    host_ref: &str,
    available_pools: &[String],
    previous: &BTreeMap<String, FulfilmentFacts>,
    probe: FulfilmentProbeContext<'_>,
    model_probes: &mut ModelProbeState,
) -> Result<BTreeMap<String, FulfilmentFacts>, String> {
    let hosts = backend.clone().using::<Host>(namespace).list().await.map_err(|error| error.to_string())?;
    let all_kinds = backend.clone().using::<FulfilmentKind>(namespace).list().await.map_err(|error| error.to_string())?.items;
    let mut kinds = Vec::new();
    for kind in all_kinds {
        if kind.metadata.deletion_timestamp.is_some() {
            continue;
        }
        if kind_belongs_to_host(&hosts.items, &kind, host_ref, "fact observation") {
            kinds.push(kind);
        }
    }
    let baselines = backend.clone().definitions::<flotilla_resources::CrewImageBaseline>(namespace);
    let mut facts = BTreeMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    // Model requests share a host budget, so kinds run serially. Rotate the
    // starting kind each pass: a persistently slow kind must not always take
    // the first slice of the host-wide deadline.
    if !kinds.is_empty() {
        let offset = (Utc::now().timestamp() as usize / FULFILMENT_CHANGE_CHECK_INTERVAL.as_secs() as usize) % kinds.len();
        kinds.rotate_left(offset);
    }
    for kind in kinds {
        let image = match &kind.spec.realisation {
            FulfilmentRealisation::DockerPerVessel { image } => match image.resolve(&baselines).await {
                Ok(image) => Some(image),
                Err(error) => {
                    warn!(kind = %kind.metadata.name, %error, "cannot probe unresolved fulfilment image");
                    continue;
                }
            },
            FulfilmentRealisation::HostDirect => None,
        };
        let pool_available = available_pools.contains(&kind.spec.pool);
        let name = kind.metadata.name;
        let current = previous
            .get(&name)
            .filter(|prior| {
                prior.image.as_ref().map(|image| &image.image_ref) == image.as_ref()
                    && prior.free_vessel_slots == (!pool_available).then_some(0)
                    && Utc::now().signed_duration_since(prior.observed_at).to_std().is_ok_and(|age| age < Duration::from_secs(300))
            })
            .cloned();
        let observed = match current {
            Some(current) => Some(current),
            None => match tokio::time::timeout_at(
                deadline.min(tokio::time::Instant::now() + Duration::from_secs(45)),
                crate::fulfilment_probe::probe_kind(
                    &kind.spec,
                    image.as_deref(),
                    pool_available,
                    probe.runner,
                    probe.env,
                    probe.scratch,
                    model_probes,
                ),
            )
            .await
            {
                Ok(Ok(observed)) => Some(observed),
                Ok(Err(error)) => {
                    warn!(kind = %name, %error, "fulfilment fact probe failed");
                    None
                }
                Err(_) => {
                    warn!(kind = %name, "fulfilment fact probe exceeded pass deadline");
                    None
                }
            },
        };
        if let Some(observed) = observed.or_else(|| previous.get(&name).cloned()) {
            facts.insert(name, observed);
        }
    }
    Ok(facts)
}

pub(super) const DEFAULT_DOCKER_IMAGE: &str = "ubuntu:24.04";

pub(super) const DEFAULT_REPO_DIR_SUFFIX: &str = "dev/flotilla-repos";
