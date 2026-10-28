//! Local and SSH host discovery and fulfilment observations.

use flotilla_discovery_api::EnvironmentBag;
use flotilla_store::DockerImageSourceStoreExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
    time::Duration,
};

use chrono::Utc;
use flotilla_config::ConfigStore;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_credentials::CredentialStore;
use flotilla_discovery::registry::ProviderRegistry;
use flotilla_discovery::DiscoveryRuntime;
use flotilla_environment::environment::EnvironmentInput;
use flotilla_paths::path_context::DaemonHostPath;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_protocol::EnvironmentId;
use flotilla_provider_runtime::execution_context::{AccountVariables, BaseLayer, ExecutionPreparer, ObservationContext, LOCALE_VARIABLES};
use flotilla_provider_runtime::host_environment::HostEnvironment;
use flotilla_provider_runtime::ChannelLabel;
use flotilla_provider_runtime::CommandRunner;
use flotilla_provider_runtime::EnvVars;
use flotilla_resources::{
    host_direct_environment_name, ConditionValue, FulfilmentFacts, FulfilmentKind, FulfilmentRealisation, Host, HostCondition,
    HostConnection, HostSpec, HostStatus, InputMeta, ModelProbeState, Platform, ResourceError, ResourceObject, AGENT_ADAPTERS_CAPABILITY,
    CREDENTIAL_EXPIRY_CAPABILITY, DOCKER_POLICY_NAME_PREFIX, HELD_CREDENTIALS_CAPABILITY, OWNING_DAEMON_CAPABILITY, PLACEMENT_CAPABILITY,
    SSH_HOST_CAPABILITY, TRANSPORT_CAPABILITY,
};
use flotilla_store::{ResourceBackend, TypedResolver};
use serde_json::json;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::fulfilment_probe::{probe_kind, DetectionTarget};
use crate::stdlib_image_layers::harness_session_environment;

use super::seed::{
    empty_meta, ensure_default_policies, ensure_host_direct_environment_exists, kind_belongs_to_host, migrate_live_placement_policies,
};
use super::ssh_host_vessels::SshHostVessels;
use super::tasks::FULFILMENT_CHANGE_CHECK_INTERVAL;
use flotilla_environment::environment::agent::host::SshAgentReach;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LocalProvisioningProfile {
    pub(super) environment_providers: Vec<flotilla_environment::environment::ProviderAdvertisement>,
    pub(super) host_id: String,
    pub(super) display_name: String,
    pub(super) repo_default_dir: String,
    pub(super) host_direct_pool: String,
    pub(super) host_direct_provider: String,
    pub(super) docker_pool: String,
    pub(super) available_pools: Vec<String>,
    pub(super) available_agent_adapters: BTreeSet<String>,
    pub(super) docker_available: bool,
    /// The vessels this host declares it can hold: an SSH host's from
    /// `hosts.toml` (#3793), the daemon's own host's Docker cap from
    /// `daemon.toml` (#4002).
    pub(super) max_vessels: Option<u32>,
}

#[derive(Clone)]
pub(super) struct SshHostProfile {
    pub(super) provisioning: LocalProvisioningProfile,
    pub(super) environment_id: EnvironmentId,
    pub(super) destination: String,
    pub(super) env_bag: EnvironmentBag,
    pub(super) runner: Arc<dyn CommandRunner>,
    /// The host's declared supplies (#3722).
    pub(super) host_environment: HostEnvironment,
    /// Where the registered families keep account state.
    pub(super) accounts: AccountVariables,
    /// The host is declared dedicated and single-tenant (#4002).
    pub(super) single_tenant: bool,
    pub(super) fulfilment_facts: Arc<RwLock<BTreeMap<String, FulfilmentFacts>>>,
    /// How its vessel agents are reached (#3792), or why they cannot be.
    pub(super) vessel_agent: Result<SshAgentReach, String>,
}

impl SshHostProfile {
    /// The host's one preparer: its probes, its vessel agents and its crews
    /// all take their base layer from here. `generation` names the
    /// directories flotilla installed for a vessel; a host probe has none.
    pub(super) fn preparer(&self, generation: Vec<String>) -> ExecutionPreparer {
        let bag = &self.env_bag;
        ExecutionPreparer::new(self.host_environment.clone(), self.accounts.clone(), bag.find_env_var("HOME"))
            .with_generation(generation)
            .with_locale(|name| bag.find_env_var(name))
            .with_settings(harness_session_environment())
    }
}

pub(super) struct BagEnvVars<'a>(pub(super) &'a EnvironmentBag);

impl EnvVars for BagEnvVars<'_> {
    fn get(&self, key: &str) -> Option<String> {
        self.0.find_env_var(key).map(ToString::to_string)
    }
}

pub(super) async fn discover_ssh_host_profiles(daemon: &Arc<InProcessDaemon>, config: &ConfigStore) -> Vec<SshHostProfile> {
    let mut profiles = Vec::new();
    for (environment_id, direct) in daemon.ssh_host_environments() {
        let profile = discover_ssh_host_profile(daemon, config, environment_id, direct).await;
        match profile {
            Ok(profile) => profiles.push(profile),
            Err(error) => warn!(%error, "failed to discover SSH host; continuing startup"),
        }
    }
    profiles
}

pub(super) async fn discover_ssh_host_profile(
    daemon: &Arc<InProcessDaemon>,
    config: &ConfigStore,
    environment_id: EnvironmentId,
    direct: flotilla_discovery::environment_manager::DirectEnvironmentState,
) -> Result<SshHostProfile, String> {
    let host_id = direct.host_id.as_ref().expect("SSH host environment has a host ID").to_string();
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
        host_direct_provider: registry
            .environment_providers
            .select_legacy_input(EnvironmentInput::HostDirect, None)
            .map(|(_, provider)| registry.environment_providers.instance_name(provider).expect("registered provider").to_string())
            .unwrap_or_else(|| "host-direct".into()),
        environment_providers: observe_provider_advertisements(&registry).await,
        host_id,
        display_name: direct.display_name.unwrap_or_else(|| environment_id.to_string()),
        repo_default_dir: format!("{home}/{DEFAULT_REPO_DIR_SUFFIX}"),
        host_direct_pool: pool,
        docker_pool: "cleat".to_string(),
        available_pools,
        available_agent_adapters: registry.agent_adapters.ids().map(ToString::to_string).collect(),
        docker_available: false,
        max_vessels: direct.max_vessels,
    };
    daemon.set_direct_environment_registry(&environment_id, Arc::clone(&registry))?;
    let destination = direct.ssh_destination.ok_or_else(|| format!("SSH host {} has no destination", provisioning.host_id))?;
    let controller_home = daemon.discovery_runtime().env.get("HOME");
    let vessel_agent = vessel_agent_reach(config, &destination, &direct.env_bag, controller_home.as_deref());
    Ok(SshHostProfile {
        provisioning,
        environment_id,
        destination,
        env_bag: direct.env_bag,
        runner: direct.runner,
        host_environment: direct.host_environment,
        accounts: daemon.discovery_runtime().factories.account_variables(),
        single_tenant: direct.single_tenant,
        fulfilment_facts: Arc::new(RwLock::new(BTreeMap::new())),
        vessel_agent,
    })
}

/// #3792: the explicit SSH recipe for the host's vessel agents, from its
/// `hosts.toml` entry. Drydock's transport reads no ambient SSH configuration,
/// so an entry without one cannot take vessels; the reason says what to set.
pub(super) fn vessel_agent_reach(
    config: &ConfigStore,
    destination: &str,
    bag: &EnvironmentBag,
    controller_home: Option<&str>,
) -> Result<SshAgentReach, String> {
    let hosts = config.load_hosts().map_err(|error| format!("read hosts.toml for {destination}: {error}"))?;
    let (label, host) = hosts
        .hosts
        .iter()
        .find(|(_, host)| host.ssh_host && flotilla_config::ssh_destination(&host.hostname, host.user.as_deref()) == destination)
        .ok_or_else(|| format!("no SSH host entry in hosts.toml reaches {destination}"))?;
    let reach = host.vessel_agent.as_ref().ok_or_else(|| {
        format!(
            "hosts.toml [hosts.{label}.vessel_agent] is not set: name the identity_file and the pinned host_key flotilla reaches its vessel agents with"
        )
    })?;
    let user = host
        .user
        .clone()
        .filter(|user| !user.is_empty())
        .or_else(|| bag.find_env_var("USER").map(str::to_string))
        .ok_or_else(|| format!("hosts.toml [hosts.{label}] needs a user for its vessel agents"))?;
    let identity_file = match (reach.identity_file.strip_prefix("~"), controller_home) {
        (Ok(rest), Some(home)) => Path::new(home).join(rest),
        (Ok(_), None) => return Err(format!("hosts.toml [hosts.{label}.vessel_agent] identity_file needs HOME to expand `~`")),
        (Err(_), _) => reach.identity_file.clone(),
    };
    Ok(SshAgentReach {
        address: reach.address.clone().unwrap_or_else(|| host.hostname.clone()),
        port: reach.port,
        user,
        host_key: reach.host_key.clone(),
        identity_file,
    })
}

pub(super) async fn register_ssh_host_resources(
    backend: &ResourceBackend,
    namespace: &str,
    owner_host_id: &str,
    profile: &SshHostProfile,
) -> Result<(), String> {
    let provisioning = &profile.provisioning;
    if provisioning.host_id == owner_host_id {
        return Err(format!("SSH host {} shares the owning daemon's Host identity", provisioning.host_id));
    }
    let hosts = backend.clone().using::<Host>(namespace);
    let mut spec = HostSpec {
        display_name: provisioning.display_name.clone(),
        connection: HostConnection::SshHost { owning_daemon: owner_host_id.to_string(), destination: profile.destination.clone() },
        max_vessels: provisioning.max_vessels,
        single_tenant: profile.single_tenant,
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
    let platform = ssh_host_platform(profile.runner.as_ref()).await;
    migrate_live_placement_policies(backend, namespace, &provisioning.host_id, &platform).await
}

pub(super) async fn ssh_host_platform(runner: &dyn CommandRunner) -> String {
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

pub(super) async fn apply_ssh_host_observation(
    daemon: &Arc<InProcessDaemon>,
    namespace: &str,
    ssh: &SshHostProfile,
    credential_store: Option<&CredentialStore>,
    vessels: Option<&SshHostVessels>,
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
                // SSH host and capacity faults do not affect the owning daemon forge reader.
                .blocks_forge_observation(false)
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
                // SSH host and capacity faults do not affect the owning daemon forge reader.
                .blocks_forge_observation(false)
                .build(),
        );
    }
    if probe_succeeded {
        let platform = ssh_host_platform(ssh.runner.as_ref()).await;
        migrate_live_placement_policies(&daemon.resource_backend(), namespace, &profile.host_id, &platform).await?;
    }
    let mut ready = ready;
    if probe_succeeded {
        let (vessel_conditions, takes_vessels) = ssh_host_vessel_conditions(vessels, ssh).await;
        ready &= takes_vessels;
        conditions.extend(vessel_conditions);
    }
    let mut fulfilment_facts = if probe_succeeded {
        let observed = ssh.fulfilment_facts.read().await.clone();
        if observed.is_empty() {
            host.status.as_ref().map(|status| status.fulfilment_facts.clone()).unwrap_or_default()
        } else {
            observed
        }
    } else {
        BTreeMap::new()
    };
    for facts in fulfilment_facts.values_mut() {
        for harness in facts.harnesses.values_mut() {
            harness.models.clear();
        }
    }
    // The owning daemon is the only placer here, so it alone counts the
    // vessels held against the host's cap (#3793).
    if let Some(cap) = profile.max_vessels {
        let occupancy = flotilla_admission::support::host_occupancy(&daemon.resource_backend(), namespace, &profile.host_id).await?;
        let probed = ssh.fulfilment_facts.read().await;
        for (kind, facts) in fulfilment_facts.iter_mut() {
            // A probe that found the kind's terminal pool missing keeps it full.
            let pool_missing = probed.get(kind).is_some_and(|probed| probed.free_vessel_slots == Some(0));
            facts.free_vessel_slots = Some(if pool_missing { 0 } else { occupancy.free_slots(cap) });
        }
    }
    let providers = match daemon.environment_registry_for_environment(&ssh.environment_id) {
        Some(registry) => observe_provider_advertisements(&registry).await,
        None => profile
            .environment_providers
            .iter()
            .cloned()
            .map(|mut provider| {
                provider.readiness =
                    flotilla_protocol::provider::ProviderReadiness::Unknown { reason: "controlling host registry unavailable".into() };
                provider
            })
            .collect(),
    };
    let status = HostStatus {
        capabilities: BTreeMap::from([
            (flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.to_string(), json!(providers)),
            (AGENT_ADAPTERS_CAPABILITY.to_string(), json!(profile.available_agent_adapters)),
            (HELD_CREDENTIALS_CAPABILITY.to_string(), json!(held_credentials)),
            (CREDENTIAL_EXPIRY_CAPABILITY.to_string(), json!(credential_expiry)),
            ("terminal_pools".to_string(), json!(profile.available_pools)),
            (SSH_HOST_CAPABILITY.to_string(), json!(true)),
            (TRANSPORT_CAPABILITY.to_string(), json!("ssh")),
            (PLACEMENT_CAPABILITY.to_string(), json!("host_direct_only")),
            (OWNING_DAEMON_CAPABILITY.to_string(), json!(daemon.local_host_id().map(|id| id.to_string()))),
        ]),
        fulfilment_facts,
        model_probes: ModelProbeState::default(),
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

/// #3792: a host this controller cannot run vessel agents on is refused at
/// placement, since its crews could neither run nor reach the daemon: no
/// agent reach, no runtime directory, or no tools for its platform. A
/// platform probe that never succeeded (an SSH blip) is diagnostic only.
pub(super) async fn ssh_host_vessel_conditions(vessels: Option<&SshHostVessels>, ssh: &SshHostProfile) -> (Vec<HostCondition>, bool) {
    // Without a crew socket server a launch is refused with that reason, as
    // for contained vessels; the host's own observation is unaffected.
    let Some(vessels) = vessels else { return (Vec::new(), true) };
    let refusals = vessels.host_refusals(ssh).await;
    let takes_vessels = !refusals.iter().any(|refusal| refusal.blocks_readiness);
    let conditions = refusals
        .into_iter()
        .map(|refusal| {
            HostCondition::builder()
                .condition_type(refusal.condition)
                .value(ConditionValue::False)
                .reason(refusal.reason)
                .message(refusal.message)
                .observed_at(Utc::now())
                .blocks_readiness(refusal.blocks_readiness)
                .blocks_forge_observation(false)
                .build()
        })
        .collect();
    (conditions, takes_vessels)
}

impl LocalProvisioningProfile {
    pub(super) fn host_direct_environment_name(&self) -> String {
        host_direct_environment_name(&self.host_id)
    }

    pub(super) fn host_direct_policy_name(&self) -> String {
        host_direct_environment_name(&self.host_id)
    }

    pub(super) fn docker_policy_name(&self) -> String {
        format!("{DOCKER_POLICY_NAME_PREFIX}{}", self.host_id)
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
    let mut registry = daemon.discovery_runtime().factories.probe_all(&local_bag, config, &probe_root, runner).await;
    #[cfg(unix)]
    super::compose_drydock(config, &mut registry, daemon.discovery_runtime().env.as_ref()).await?;
    Ok(Arc::new(registry))
}

// Per-vessel providers that bring their own runtime do not need a host pool.
pub(super) fn crew_pool_available(host_has_pool: bool, per_vessel: bool, receives_host_tools: bool) -> bool {
    host_has_pool || (per_vessel && !receives_host_tools)
}

pub(super) async fn build_local_profile(
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
    // General provisioning availability follows per_vessel capability data:
    // dummy brings its own crew runtime, while other instances need host cleat.
    // This retains the legacy seed-policy availability flag. Admission uses
    // individual advertisements. Stored-Docker recovery in environments.rs/state.rs is deliberately
    // narrower because it reads docker_container_id. Remove that legacy-input
    // bridge and its recovery filters together at the generated-table move (#3108).
    let docker_available = local_registry.environment_providers.iter().any(|(_, provider)| {
        provider.capabilities().per_vessel
            && crew_pool_available(
                local_registry.terminal_pools.contains_key(&docker_pool),
                provider.capabilities().per_vessel,
                provider.receives_host_tools(),
            )
    });
    let available_agent_adapters = local_registry.agent_adapters.ids().map(ToString::to_string).collect();

    Ok(LocalProvisioningProfile {
        host_direct_provider: local_registry
            .environment_providers
            .select_legacy_input(EnvironmentInput::HostDirect, None)
            .map(|(_, provider)| local_registry.environment_providers.instance_name(provider).expect("registered provider").to_string())
            .unwrap_or_else(|| "host-direct".into()),
        environment_providers: observe_provider_advertisements(local_registry).await,
        host_id,
        display_name: daemon.host_name().to_string(),
        repo_default_dir,
        host_direct_pool,
        docker_pool,
        available_pools,
        available_agent_adapters,
        docker_available,
        // The composition root sets the daemon's own cap from `daemon.toml`.
        max_vessels: None,
    })
}

/// Re-observe prerequisites on the controlling host, including credential loss.
pub(super) async fn observe_provider_advertisements(
    registry: &ProviderRegistry,
) -> Vec<flotilla_protocol::provider::ProviderAdvertisement> {
    registry
        .environment_providers
        .advertisements()
        .await
        .into_iter()
        .map(|mut provider| {
            if provider.readiness == flotilla_protocol::provider::ProviderReadiness::Ready
                && provider.capabilities.receives_host_tools
                && !registry.terminal_pools.contains_key("cleat")
            {
                provider.readiness =
                    flotilla_protocol::provider::ProviderReadiness::MissingTool { reason: "cleat required for host tool injection".into() };
            }
            provider
        })
        .collect()
}

pub(super) struct FulfilmentProbeContext<'a> {
    pub(super) providers: &'a ProviderRegistry,
    pub(super) runner: &'a dyn CommandRunner,
    pub(super) env: &'a dyn EnvVars,
    pub(super) scratch: &'a Path,
    /// The host's prepared base layer and isolation.
    pub(super) observation: &'a ObservationContext,
}

/// The daemon's own host declares its environment through the daemon
/// process: its probes take that `PATH` and locale as their base layer.
pub(super) fn local_observation(discovery: &DiscoveryRuntime) -> Result<ObservationContext, String> {
    let declared = std::iter::once("PATH").chain(LOCALE_VARIABLES.iter().copied());
    let base = BaseLayer::declared(
        declared.filter_map(|name| discovery.env.get(name).map(|value| (name.to_string(), value))),
        &discovery.factories.account_variables(),
    )?;
    Ok(ObservationContext::new(base, &discovery.factories.account_variables()))
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
    *model_probes = ModelProbeState::default();
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
    // The kinds listed above are this store's own records, never replicas, so
    // writing their observed grants stays with the daemon that authors them.
    let kind_store = backend.clone().using::<FulfilmentKind>(namespace);
    // A capped kind's free slots are counted from the vessels placed on the
    // host, as admission counts them (#4002). An SSH host's are counted
    // when its observation is published (#3793).
    let host_spec =
        hosts.items.iter().find(|host| host.metadata.name == host_ref).map(|host| &host.spec).filter(|host| host.connection.is_daemon());
    let occupancy = match host_spec.and_then(|host| host.max_vessels) {
        Some(_) => Some(flotilla_admission::support::host_occupancy(backend, namespace, host_ref).await?),
        None => None,
    };
    let mut facts = BTreeMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    // Rotate the starting kind each pass: a persistently slow kind must not always take
    // the first slice of the host-wide deadline.
    if !kinds.is_empty() {
        let offset = (Utc::now().timestamp() as usize / FULFILMENT_CHANGE_CHECK_INTERVAL.as_secs() as usize) % kinds.len();
        kinds.rotate_left(offset);
    }
    for kind in kinds {
        let provider_kind = match kind.spec.realisation {
            FulfilmentRealisation::HostDirect => EnvironmentInput::HostDirect,
            FulfilmentRealisation::DockerPerVessel { .. } => EnvironmentInput::Docker,
            FulfilmentRealisation::CloudSandboxPerVessel { .. } => continue,
        };
        let provider = probe.providers.environment_providers.for_legacy_input(provider_kind).map(|(_, provider)| provider);
        let pool_available = crew_pool_available(
            available_pools.contains(&kind.spec.pool),
            provider.is_some_and(|provider| provider.capabilities().per_vessel),
            provider.is_none_or(|provider| provider.receives_host_tools()),
        );
        // A missing terminal pool keeps the kind full.
        let free_vessel_slots = match (pool_available, host_spec.and_then(|host| host.vessel_cap(&kind.spec.realisation)), &occupancy) {
            (false, _, _) => Some(0),
            (true, Some(cap), Some(occupancy)) => Some(occupancy.free_slots(cap)),
            _ => None,
        };
        let image = match &kind.spec.realisation {
            FulfilmentRealisation::DockerPerVessel { image: Some(image) } => match image.resolve(&baselines).await {
                Ok(image) => Some(image),
                Err(error) => {
                    warn!(kind = %kind.metadata.name, %error, "cannot probe unresolved fulfilment image");
                    continue;
                }
            },
            // A composed kind has no image until admission resolves each
            // vessel's layers; resolution, not a probe, covers its image needs
            // (ADR 0062). Publish its capacity facts only.
            FulfilmentRealisation::DockerPerVessel { image: None } => {
                facts.insert(kind.metadata.name, FulfilmentFacts { observed_at: Utc::now(), free_vessel_slots, ..Default::default() });
                continue;
            }
            FulfilmentRealisation::HostDirect => None,
            FulfilmentRealisation::CloudSandboxPerVessel { .. } => continue,
        };
        let Some(provider) = provider else {
            debug!(kind = %kind.metadata.name, ?provider_kind, "cannot observe fulfilment facts without an unambiguous environment provider");
            // Absence or ambiguity cannot attest a runtime endpoint. Replace
            // stale affirmative evidence with a fresh unknown observation.
            // Unknown is not an observation, so the kind's grants stay as
            // they are.
            let unknown =
                FulfilmentFacts { image: image.map(Into::into), observed_at: Utc::now(), free_vessel_slots, ..Default::default() };
            facts.insert(kind.metadata.name, unknown);
            continue;
        };
        let name = kind.metadata.name.clone();
        // Free slots are set afresh below, so a recent probe stays reusable
        // while vessels come and go.
        let current = previous
            .get(&name)
            .filter(|prior| {
                prior.image.as_ref().map(|image| &image.image_ref) == image.as_ref()
                    && Utc::now().signed_duration_since(prior.observed_at).to_std().is_ok_and(|age| age < Duration::from_secs(300))
            })
            .cloned();
        let observed = match current {
            Some(current) => Some(current),
            None => match tokio::time::timeout_at(
                deadline.min(tokio::time::Instant::now() + Duration::from_secs(45)),
                probe_kind(
                    &kind.spec,
                    DetectionTarget { image: image.as_deref(), provider: provider.as_ref(), context: probe.observation },
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
        if let Some(mut observed) = observed.or_else(|| previous.get(&name).cloned()) {
            for harness in observed.harnesses.values_mut() {
                harness.models.clear();
            }
            observed.free_vessel_slots = free_vessel_slots;
            observe_grants(&kind_store, &kind, &observed).await;
            facts.insert(name, observed);
        }
    }
    Ok(facts)
}

/// Grants come from observation (#4002): a host-direct kind holds a GUI
/// session grant exactly while its host's facts report one. A failed write
/// is retried on the next pass.
async fn observe_grants(kinds: &TypedResolver<FulfilmentKind>, kind: &ResourceObject<FulfilmentKind>, facts: &FulfilmentFacts) {
    let mut spec = kind.spec.clone();
    if spec.observe_grants(facts) {
        if let Err(error) = kinds.update(&InputMeta::from(&kind.metadata), &kind.metadata.resource_version, &spec).await {
            warn!(kind = %kind.metadata.name, %error, "failed to bring fulfilment grants in line with observation");
        }
    }
}

pub(super) const DEFAULT_DOCKER_IMAGE: &str = "ubuntu:24.04";

pub(super) const DEFAULT_REPO_DIR_SUFFIX: &str = "dev/flotilla-repos";
