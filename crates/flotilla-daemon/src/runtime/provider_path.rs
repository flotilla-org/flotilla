//! The generic provider path (#3981): an Environment that is not a Docker one is
//! prepared and provisioned by its own provider. Where the provider's vessels
//! receive the generation's tools, the vessel layer (#3956 Q4) then lands
//! through the vessel agent alone: the tools, the crew socket and the
//! environment that names them, added to what the provider configured. Interior
//! discovery registers the vessel, and its image is checked against the typed
//! probe needs its Environment declares. Nothing here names a provider or
//! interprets a reach.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_discovery_api::EnvironmentBag;
use flotilla_environment::environment::{
    EnvironmentHandle, EnvironmentProvisionError, EnvironmentToolAssetKind, EnvironmentVariableUpdate, VesselAgent, DAEMON_SOCKET_ENV,
};
use flotilla_protocol::EnvironmentId;
use flotilla_provider_runtime::{
    execution_context::{AccountVariables, BaseLayer, ObservationContext, LOCALE_VARIABLES},
    ChannelLabel,
};
use flotilla_provisioning::reconcilers::{CloudProvisioning, EnvironmentProvisioning, LocalProvisioning};
use flotilla_resources::EnvironmentSpec;
use sha2::Digest;

use super::docker::{discard_failed_environment, probe_provisioned_environment};
use super::environments::ActiveProvisionedEnvironment;
use super::state::ControllerRuntimeState;
use crate::environment_tools::{EnvironmentToolContext, EnvironmentToolProviderKind, DOCKER_PROVIDER_KIND};

/// The crew socket's file name in the vessel.
const CREW_SOCKET: &str = "flotilla.sock";
/// The contained tool layout, under `/opt/flotilla`: a layout the tool
/// provisioner keys by its Docker kind, not the vessel's provider.
const CONTAINED_TOOL_LAYOUT: EnvironmentToolProviderKind = DOCKER_PROVIDER_KIND;
/// The `PATH` a vessel's tools prepend to when the vessel has none of its own.
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Provision `name` through its own provider, then deliver its vessel layer.
pub(super) async fn provision(
    state: &Arc<ControllerRuntimeState>,
    name: &str,
    spec: &EnvironmentSpec,
) -> Result<EnvironmentProvisioning, EnvironmentProvisionError> {
    let provider = state.provider_for_environment(name).await?;
    let prepared = provider.prepare(spec, &Default::default()).await?;
    let id = EnvironmentId::new(name);
    let handle = provider.provision(id.clone(), &prepared, Default::default()).await?;
    // A cloud backing's Ready status records its provider's facts: a provider
    // that gives none fails here, before the vessel layer, not at Ready.
    let described = match &spec.cloud_sandbox {
        None => None,
        Some(_) => match handle.provider_descriptor() {
            Some(descriptor) => Some(descriptor),
            None => {
                let error = format!("cloud provider for {name} reported no provider descriptor");
                return Err(discard_failed_environment(
                    &handle,
                    state.credential_store.as_deref(),
                    state.agent_material.as_deref(),
                    name,
                    error,
                )
                .await);
            }
        },
    };
    if provider.receives_host_tools() {
        let delivered = async {
            deliver_vessel_layer(state, &id, spec, &handle).await?;
            let (bag, registry) = probe_provisioned_environment(state, &id, &handle).await?;
            if let Some(cloud) = &spec.cloud_sandbox {
                let factories = &state.daemon.discovery_runtime().factories;
                let accounts = factories.account_variables();
                verify_cloud_image_needs(
                    &cloud.probe_needs,
                    &cloud.image,
                    &handle,
                    &bag,
                    &accounts,
                    &factories.harnesses(&GuestProbeEnvironment(&bag)),
                )
                .await?;
            }
            state
                .daemon
                .register_provisioned_environment(id.clone(), Arc::clone(&handle), bag, Some(registry))
                .map_err(|error| format!("failed to register provisioned environment {id}: {error}"))
        }
        .await;
        if let Err(error) = delivered {
            // A retained backing keeps its socket for the retry; a discarded one does not.
            let error =
                discard_failed_environment(&handle, state.credential_store.as_deref(), state.agent_material.as_deref(), name, error).await;
            if let (EnvironmentProvisionError::Failed(_), Some(sockets)) = (&error, &state.environment_sockets) {
                sockets.remove(&id).await?;
            }
            return Err(error);
        }
    }
    let facts = match described {
        Some(described) => EnvironmentProvisioning::Cloud(CloudProvisioning {
            backing_id: handle.backing_id().to_string(),
            image_ref: handle.image().as_str().to_string(),
            // Read again, so the descriptor carries the agent's latest connection.
            provider_descriptor: Box::new(handle.provider_descriptor().unwrap_or(described)),
        }),
        None => EnvironmentProvisioning::Local(LocalProvisioning {
            backing_id: Some(handle.backing_id().to_string()),
            // A backing that reports its image's digest names the image it runs.
            image_ref: handle.local_image_id().map(|_| handle.image().as_str().to_string()),
            local_image_id: handle.local_image_id().map(str::to_owned),
            registry_digest: handle.registry_digest().map(str::to_owned),
            configured_limits: None,
        }),
    };
    state.provisioned_environments.lock().await.insert(handle.backing_id().to_string(), ActiveProvisionedEnvironment { handle });
    Ok(facts)
}

/// The vessel layer, through the vessel agent: the generation's tools pushed,
/// the crew socket served in the vessel from this environment's own socket
/// here, and the environment naming both, added to what the provider
/// configured. Each step is idempotent, so re-adoption delivers it again; a
/// step that fails on a retained backing leaves the earlier ones for the retry.
async fn deliver_vessel_layer(
    state: &ControllerRuntimeState,
    id: &EnvironmentId,
    spec: &EnvironmentSpec,
    handle: &EnvironmentHandle,
) -> Result<(), String> {
    let sockets = state.environment_sockets.as_ref().ok_or("EnvironmentSocketUnavailable: a vessel layer requires a crew socket server")?;
    let controller = sockets.prepare_forwarded(id.clone()).await?;
    let agent = handle.agent();
    // The crew socket lives with the vessel's other writable state, wherever its agent keeps that.
    let digest = format!("{:x}", sha2::Sha256::digest(id.as_str().as_bytes()));
    let directory = agent.writable_base().await?.join("crew-sockets").join(&digest[..16]);
    agent.create_dir(&directory, 0o700).await?;
    let guest = directory.join(CREW_SOCKET);
    let cpus = spec.cloud_sandbox.as_ref().map(|cloud| cloud.size.cpus as usize);
    let mut tools = state.environment_tools.prepare(CONTAINED_TOOL_LAYOUT, id.as_str(), &DeclaredSize { state, cpus }).await?;
    let mut environment = BTreeMap::new();
    for tool in &mut tools {
        // The crew socket is the layer's own forward, not a pushed file.
        tool.assets.retain(|asset| asset.kind != EnvironmentToolAssetKind::UnixSocket);
        for update in &tool.environment {
            apply(&mut environment, update, &guest.to_string_lossy(), agent.as_ref()).await?;
        }
    }
    agent.forward_controller_socket(&guest, None, controller.as_path()).await.map_err(|error| format!("forward crew socket: {error}"))?;
    agent.push_tools(&tools).await.map_err(|error| format!("push vessel tools: {error}"))?;
    agent.extend_environment(environment).await.map_err(|error| format!("configure vessel environment: {error}"))
}

/// One tool's update to the vessel's environment. The daemon socket variable
/// always names the vessel's own crew socket. A path prepends to the vessel's
/// own value, from its image or its provider, as its agent reports it; once per
/// variable, since later updates prepend to the value already gathered.
async fn apply(
    environment: &mut BTreeMap<String, String>,
    update: &EnvironmentVariableUpdate,
    socket: &str,
    agent: &dyn VesselAgent,
) -> Result<(), String> {
    match update {
        EnvironmentVariableUpdate::Set { name, .. } if name == DAEMON_SOCKET_ENV => {
            environment.insert(name.clone(), socket.to_string());
        }
        EnvironmentVariableUpdate::Set { name, value, .. } => {
            environment.insert(name.clone(), value.clone());
        }
        EnvironmentVariableUpdate::PrependPath { name, value } => {
            let existing = match environment.get(name) {
                Some(existing) => existing.clone(),
                None => vessel_variable(agent, name).await?,
            };
            // Only a vessel with no `PATH` at all takes the conventional one.
            let existing = if existing.is_empty() && name == "PATH" { DEFAULT_PATH.to_string() } else { existing };
            let joined = if existing.is_empty() {
                value.clone()
            } else if existing.split(':').any(|entry| entry == value) {
                existing
            } else {
                format!("{value}:{existing}")
            };
            environment.insert(name.clone(), joined);
        }
    }
    Ok(())
}

/// The value `name` has for the vessel's commands, empty where it is unset.
async fn vessel_variable(agent: &dyn VesselAgent, name: &str) -> Result<String, String> {
    let commands = agent.commands();
    let value = commands
        .run("sh", &["-c", "printenv \"$1\" || :", "sh", name], Path::new("/"), &ChannelLabel::Default)
        .await
        .map_err(|error| format!("read vessel {name}: {error}"))?;
    Ok(value.trim_end_matches('\n').to_string())
}

/// Build limits from the backing's declared size, or this host's where it declares none.
struct DeclaredSize<'a> {
    state: &'a ControllerRuntimeState,
    cpus: Option<usize>,
}

#[async_trait]
impl EnvironmentToolContext for DeclaredSize<'_> {
    async fn rust_build_jobs(&self) -> Result<usize, String> {
        match self.cpus {
            Some(cpus) => Ok(cpus),
            None => self.state.rust_build_jobs(&self.state.local_host_ref).await,
        }
    }
}

struct GuestProbeEnvironment<'a>(&'a EnvironmentBag);
impl flotilla_provider_runtime::EnvVars for GuestProbeEnvironment<'_> {
    fn get(&self, key: &str) -> Option<String> {
        self.0.find_env_var(key).map(str::to_owned)
    }
}

/// The image check: the vessel's image provides every need its Environment
/// declares for a guest probe.
pub(super) async fn verify_cloud_image_needs(
    needs: &BTreeSet<flotilla_resources::CapabilityNeed>,
    image: &str,
    handle: &EnvironmentHandle,
    bag: &EnvironmentBag,
    accounts: &AccountVariables,
    harnesses: &[flotilla_agent::adapter::Harness],
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
        crate::fulfilment_probe::DetectionTarget { image: None, provider: detector.as_ref(), context: &observation, harnesses },
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
