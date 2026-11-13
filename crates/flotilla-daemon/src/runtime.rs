//! Daemon composition, startup restoration, and resource-controller wiring.

mod access_layer;
mod account_access;
pub(crate) mod archive;
mod checkouts;
mod credential_delivery;
mod credentials;
mod discovery;
mod docker;
mod environments;
mod health;
mod ports;
mod primitives;
mod provider_path;
mod remotes;
mod role_consumer;
mod seed;
pub(crate) mod ssh_host_vessels;
mod state;
mod tasks;
mod terminal;
use std::{collections::BTreeSet, path::PathBuf, sync::Arc, time::Duration};
pub(crate) use terminal::{DELIVERY_CONFIRMATION_POLL, DELIVERY_CONFIRMATION_STABLE_FOR};

use chrono::Utc;
use flotilla_admission::reconcilers::convoy_ensure::EnsureReconciler;
use flotilla_config::ConfigStore;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_credentials::{codex_central_auth_path, AgentMaterialRegistry, CodexCentralRefresher, CredentialStore};
use flotilla_crew::convoy::ConvoyReconciler;
use flotilla_crew::reconcilers::vessel::{ConvoyVesselWake, CrewSeats, VesselPlanning};
use flotilla_discovery::registry::ProviderRegistry;
use flotilla_paths::path_context::DaemonHostPath;
use flotilla_protocol::CanonicalHostId;
use flotilla_provisioning::engine::ProvisioningLifecycles;
use flotilla_provisioning::reconcilers::{ForgeDefaultBranchResolver, RepositoryReconciler};
use flotilla_resources::LANDING_EVIDENCE_TTL;
use flotilla_resources::{host_direct_environment_name, Convoy, Forge, Host, Repository, Resource, ResourceError, Vessel};
use flotilla_store::{controller::ControllerLoop, ResourceBackend};
use tokio::{sync::watch, task::JoinHandle};
use tokio_util::task::AbortOnDropHandle;
use tracing::{debug, error, info, warn};

use self::credential_delivery::CredentialDelivery;
use self::credentials::{RuntimeSessionCapabilities, RuntimeWorkCredentialReconciler};
use self::discovery::{
    apply_ssh_host_observation, build_local_profile, discover_ssh_host_profiles, probe_local_provider_registry, register_ssh_host_resources,
};
use self::docker::DockerControllerRuntime;
use self::environments::reconcile_provisioned_environments;
use self::health::{apply_host_heartbeat_with_credentials, spawn_projection_parity_task, DaemonHealthIdentity, RuntimeHealth};
use self::ports::{
    ChainedForgeDefaultBranchResolver, GhForgeDefaultBranchResolver, RoutingCheckoutRuntime, RoutingCloneRuntime, RuntimeOperatorReconciler,
};
use self::seed::register_startup_resources;
use self::state::ControllerRuntimeState;
use self::tasks::{
    run_artifact_gc, run_checkout_archive_gc, spawn_aggregator_task, spawn_convoy_ensure_reconciler_task, spawn_dispatch_reconciler_task,
    spawn_environment_orphan_sweep_task, spawn_heartbeat_task_with_credentials, spawn_host_description_projection_task,
    spawn_local_fulfilment_probe_task, spawn_manifest_reconciler_task, spawn_named_controller, spawn_periodic_task,
    spawn_provisioned_environment_reconciliation_task, spawn_sleep_inhibitor_task, spawn_ssh_fulfilment_probe_task,
    spawn_startup_restoration, spawn_vessel_requests, CheckoutArchiveRoot, CheckoutArchiveSweep, PeriodicTaskStart,
    MANIFEST_RECONCILE_INTERVAL,
};
use self::terminal::TerminalControllerRuntime;
use crate::server::environment_sockets::EnvironmentSocketService;
use crate::{
    blob_store::TieredBlobStore,
    resource_manifest::manifest_root_name,
    startup::{phase, result_phase, retry_phase},
    supervisor::ControllerSupervision,
};
pub(crate) use tasks::manifest_reconciler_enabled;
pub use tasks::spawn_pending_supervisor_turn_task;
pub use tasks::spawn_pending_supervisor_turn_task_with_watches;
#[cfg(test)]
pub(crate) use tasks::wait_for_listening;

#[derive(Debug, Clone, bon::Builder)]
pub struct RuntimeOptions {
    pub namespace: String,
    pub heartbeat_interval: Duration,
    pub controller_resync_interval: Duration,
    /// How often the local host's fulfilment facts are re-probed. Harnesses
    /// shorten it so a kind seeded after startup is observed promptly.
    #[builder(default = tasks::FULFILMENT_CHANGE_CHECK_INTERVAL)]
    pub fulfilment_probe_interval: Duration,
    pub controller_supervision: ControllerSupervision,
    pub start_controllers: bool,
    /// Socket servers release restoration once their accept loop is ready.
    /// In-process runtimes omit the gate and restore in the background immediately.
    pub startup_ready: Option<watch::Receiver<bool>>,
    pub environment_sockets: Option<EnvironmentSocketService>,
    /// flotillad's hook endpoint: host-direct Claude Code crews deliver their hooks over
    /// HTTP while it has one, and keep command hooks otherwise.
    pub hook_endpoint: Option<crate::server::HookEndpoint>,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            namespace: flotilla_core::in_process::DEFAULT_PROVISIONING_NAMESPACE.to_string(),
            heartbeat_interval: Duration::from_secs(30),
            controller_resync_interval: Duration::from_secs(60),
            fulfilment_probe_interval: tasks::FULFILMENT_CHANGE_CHECK_INTERVAL,
            controller_supervision: ControllerSupervision::default(),
            start_controllers: true,
            startup_ready: None,
            environment_sockets: None,
            hook_endpoint: None,
            // Codex only proactively refreshes within 5 minutes of expiry; a
            // central login's access token lives far longer than that, so a
            // conservative fixed cadence comfortably inside that window
            // (rather than tracking each token's own `exp`) is sufficient.
        }
    }
}

pub struct DaemonRuntime {
    tasks: Vec<JoinHandle<()>>,
    pub blob_store: Arc<TieredBlobStore>,
    /// Per-kind controller loop load metrics (#3206).
    pub controller_metrics: Arc<flotilla_store::load_metrics::ControllerMetricsRegistry>,
    /// Set by `shutdown` so `Drop` can tell an intended stop from a runtime
    /// that vanished while the daemon was meant to keep working.
    stop_expected: bool,
}

impl DaemonRuntime {
    pub async fn start(
        daemon: Arc<InProcessDaemon>,
        config: Arc<ConfigStore>,
        daemon_socket_path: Option<PathBuf>,
    ) -> Result<Self, String> {
        Self::start_with_options(daemon, config, daemon_socket_path, RuntimeOptions::default()).await
    }

    pub async fn start_with_options(
        daemon: Arc<InProcessDaemon>,
        config: Arc<ConfigStore>,
        daemon_socket_path: Option<PathBuf>,
        options: RuntimeOptions,
    ) -> Result<Self, String> {
        // Boxed: initialisation composes the whole daemon, and its future is too large for a
        // caller's stack (a 2 MiB test thread overflowed polling it in place).
        result_phase("initialize_runtime", Box::pin(Self::initialize_with_options(daemon, config, daemon_socket_path, options))).await
    }

    async fn initialize_with_options(
        daemon: Arc<InProcessDaemon>,
        config: Arc<ConfigStore>,
        daemon_socket_path: Option<PathBuf>,
        options: RuntimeOptions,
    ) -> Result<Self, String> {
        if let Some(path) = daemon_socket_path.as_ref() {
            phase("set_daemon_socket_path", daemon.set_daemon_socket_path(path.clone())).await;
        }
        phase("set_provisioning_namespace", daemon.set_provisioning_namespace(options.namespace.clone())).await;
        let aggregator_projection_state = phase("aggregator_projection_state", daemon.aggregator_projection_state()).await;
        let daemon_config = config.load_daemon_config()?;
        // ADR 0064 (#3108): Message and the provisioning lifecycles run on the decided store.
        result_phase("open_message_primitives", open_message_primitives(&daemon)).await?;
        let (provisioning, lifecycle_followers) = open_provisioning_lifecycles(&daemon).await?;
        daemon.require_lifecycle_reads_installed()?;
        let manifests = daemon_config.manifests;
        let relay = daemon_config.relay;
        let forge_default_branches = crate::dummy_fleet::dummy_forge_default_branches(daemon_config.dummy_fleet.as_ref());
        let blob_store = Arc::new(TieredBlobStore::from_config(config.state_dir().as_path(), &daemon_config.blob_stores)?);
        phase(
            "set_brief_artifact_writer",
            daemon.set_brief_artifact_writer(Arc::new(crate::artifact::SystemBriefArtifactWriter {
                backend: daemon.resource_backend(),
                blobs: Arc::clone(&blob_store),
                retention_days: daemon_config.artifact_retention_days.get("brief").copied().unwrap_or(30),
            })),
        )
        .await;

        // Host binary identity is a composition-root fact. Resolve the real
        // companion of this running daemon, never a fleet shell launcher whose
        // bytes would be pushed as if they were the drydock executable.
        #[cfg(target_os = "linux")]
        if let Ok(executable) = std::env::current_exe() {
            if let Some(parent) = executable.parent() {
                let agent = parent.join("drydock-agent");
                if agent.is_file() {
                    if let Some(bag) = daemon.local_environment_bag() {
                        daemon.environment_manager().replace_local_environment_bag(
                            flotilla_discovery_api::EnvironmentBag::new().with(flotilla_discovery_api::EnvironmentAssertion::binary("drydock-agent", agent)).extend(bag.assertions().iter().filter(|assertion| !matches!(assertion, flotilla_discovery_api::EnvironmentAssertion::BinaryAvailable { name, .. } if name == "drydock-agent")).cloned()),
                        )?;
                    }
                }
            }
        }
        let local_registry = retry_phase("probe_local_provider_registry", || probe_local_provider_registry(&daemon, &config)).await?;
        let mut profile = retry_phase("build_local_profile", || build_local_profile(&daemon, &local_registry)).await?;
        profile.max_vessels = daemon_config.max_vessels;
        let host_direct_environment_name = host_direct_environment_name(&profile.host_id);
        let mut archive_roots = vec![CheckoutArchiveRoot {
            env_ref: host_direct_environment_name.clone(),
            path: PathBuf::from(&profile.repo_default_dir).join(".flotilla-archives"),
        }];
        match config.load_observation_roots() {
            Ok(roots) => {
                // Observed roots are checkout paths; worktree archives live beside their parent clones.
                archive_roots.extend(roots.into_iter().filter_map(|root| {
                    root.as_path().parent().map(|parent| CheckoutArchiveRoot {
                        env_ref: host_direct_environment_name.clone(),
                        path: parent.join(".flotilla-archives"),
                    })
                }));
            }
            Err(error) => warn!(%error, "checkout archive sweep could not load observation roots"),
        }
        let ssh_profiles = phase("discover_ssh_host_profiles", discover_ssh_host_profiles(&daemon, &config)).await;
        daemon.set_admission_free_space_path(PathBuf::from(&profile.repo_default_dir));
        let credential_store = Arc::new(CredentialStore::new_with_http(
            daemon.resource_backend(),
            &options.namespace,
            Arc::clone(&daemon.discovery_runtime().env),
            daemon.local_environment_bag().ok_or_else(|| "local environment bag unavailable".to_string())?,
            daemon.local_command_runner().ok_or_else(|| "local command runner unavailable".to_string())?,
            Arc::clone(&daemon.discovery_runtime().http),
            config.state_dir().as_path().to_path_buf(),
        ));
        // Cleanup must precede admitting commands that can mint new staging files.
        if let Err(error) = phase("cleanup_stale_github_app_token_files", credential_store.cleanup_stale_github_app_token_files()).await {
            warn!(%error, "failed to clean up stale GitHub App token staging files");
        }
        let agent_material = Arc::new(AgentMaterialRegistry::new(Arc::clone(&daemon.discovery_runtime().env)));
        let health = DaemonHealthIdentity {
            // Each daemon start is a new generation.
            generation: Some(uuid::Uuid::new_v4().to_string()),
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: Utc::now(),
        };
        phase(
            "set_local_placement_capabilities",
            daemon.set_local_placement_capabilities(&profile.available_agent_adapters, &profile.available_pools),
        )
        .await;
        let runtime_health = RuntimeHealth::default()
            .with_restart_history_dir(config.state_dir().as_path().to_path_buf())
            .with_blob_store(Arc::clone(&blob_store));
        runtime_health.host_observations.record_provider_advertisements(profile.environment_providers.clone()).await;

        retry_phase("quarantine_undecodable_stored_objects", || async {
            flotilla_store::quarantine_undecodable_stored_objects(&daemon.resource_backend(), &options.namespace).await
        })
        .await
        .map_err(|error| format!("scan stored resources for decode quarantine: {error}"))?;
        retry_phase("register_startup_resources", || register_startup_resources(&daemon, &options.namespace, &profile)).await?;
        let registered_image_caches = local_registry
            .environment_providers
            .iter()
            .filter(|(_, provider)| provider.local_image_cache().is_some())
            .filter_map(|(_, provider)| local_registry.environment_providers.instance_name(provider).map(String::from))
            .collect::<BTreeSet<_>>();
        retry_phase("prune_unregistered_image_caches", || async {
            flotilla_provisioning::image_inventory::prune_unregistered_caches(
                &daemon.resource_backend(),
                &options.namespace,
                &profile.host_id,
                &registered_image_caches,
            )
            .await
        })
        .await?;
        // #3792: SSH hosts run each vessel through its own drydock agent,
        // whose guest forward carries crews to a per-vessel environment socket.
        let ssh_host_vessels = options.environment_sockets.as_ref().map(|sockets| {
            use sha2::Digest;
            let state = config.state_dir().as_path().to_string_lossy().into_owned();
            // Short, like Docker's agent root: attach sockets live below it.
            let local = std::path::Path::new("/tmp").join(&format!("fv-{:x}", sha2::Sha256::digest(state.as_bytes()))[..23]);
            Arc::new(ssh_host_vessels::SshHostVessels::new(Arc::new(ssh_host_vessels::SshVesselAgents::default()), sockets.clone(), local))
        });
        let mut registered_ssh_profiles = Vec::new();
        for ssh in ssh_profiles {
            if let Err(error) = retry_phase("register_ssh_host_resources", || async {
                register_ssh_host_resources(
                    &daemon.resource_backend(),
                    daemon.lifecycle_reads(),
                    &options.namespace,
                    &profile.host_id,
                    &ssh,
                )
                .await
            })
            .await
            {
                warn!(host = %ssh.provisioning.host_id, %error, "failed to register SSH host; continuing startup");
                continue;
            }
            if let Err(error) = retry_phase("apply_ssh_host_observation", || {
                apply_ssh_host_observation(&daemon, &options.namespace, &ssh, Some(&credential_store), ssh_host_vessels.as_deref())
            })
            .await
            {
                warn!(host = %ssh.provisioning.host_id, %error, "failed to observe SSH host; continuing startup");
            }
            registered_ssh_profiles.push(ssh);
        }
        let ssh_profiles = registered_ssh_profiles;
        retry_phase("publish_local_host_heartbeat", || {
            apply_host_heartbeat_with_credentials(&daemon, &options.namespace, &profile, Some(&credential_store), &health, &runtime_health)
        })
        .await?;

        let mut tasks = lifecycle_followers;
        tasks.extend([
            spawn_local_fulfilment_probe_task(
                Arc::clone(&daemon),
                options.namespace.clone(),
                profile.clone(),
                Arc::clone(&local_registry),
                config.state_dir().as_path().join("probe-cwd"),
                runtime_health.host_observations.clone(),
                options.fulfilment_probe_interval,
            ),
            tokio::spawn(Arc::clone(&blob_store).run_sync()),
            tokio::spawn(run_artifact_gc(daemon.resource_backend(), options.namespace.clone(), Arc::clone(&blob_store))),
            tokio::spawn(run_checkout_archive_gc(CheckoutArchiveSweep {
                daemon: Arc::clone(&daemon),
                catalog_path: config.state_dir().as_path().join("checkout-archive-roots.json"),
                roots: archive_roots,
                retention_days: daemon_config.checkout_archive_retention_days,
            })),
            spawn_heartbeat_task_with_credentials(
                Arc::clone(&daemon),
                options.namespace.clone(),
                profile.clone(),
                Arc::new(Some(Arc::clone(&credential_store))),
                health.clone(),
                runtime_health.clone(),
                options.heartbeat_interval,
            ),
            spawn_host_description_projection_task(Arc::clone(&daemon), options.namespace.clone(), options.heartbeat_interval),
            spawn_projection_parity_task(
                daemon.resource_backend(),
                options.namespace.clone(),
                aggregator_projection_state.clone(),
                runtime_health.clone(),
                options.heartbeat_interval,
            ),
            spawn_sleep_inhibitor_task(
                daemon.resource_backend(),
                options.namespace.clone(),
                profile.host_id.clone(),
                options.controller_supervision.clone(),
                runtime_health.clone(),
            ),
            spawn_aggregator_task(
                Arc::clone(&daemon),
                options.namespace.clone(),
                aggregator_projection_state,
                options.controller_supervision.clone(),
                runtime_health.clone(),
            ),
        ]);
        if let Some((shipper, service)) = archive_shipper(&daemon, &daemon_config.blob_stores, &runtime_health)? {
            tasks.push(tokio::spawn(shipper.run(service)));
        }
        if let Some(relay) = relay {
            tasks.push(crate::event_relay::spawn(Arc::clone(&daemon), relay, config.state_dir().as_path().to_path_buf())?);
        }
        for ssh in &ssh_profiles {
            tasks.push(spawn_ssh_fulfilment_probe_task(Arc::clone(&daemon), options.namespace.clone(), ssh.clone()));
            let daemon = Arc::clone(&daemon);
            let namespace = options.namespace.clone();
            let ssh = ssh.clone();
            let credential_store = Arc::clone(&credential_store);
            let ssh_host_vessels = ssh_host_vessels.clone();
            tasks.push(spawn_periodic_task(options.heartbeat_interval, PeriodicTaskStart::AfterInterval, move || {
                let daemon = Arc::clone(&daemon);
                let namespace = namespace.clone();
                let ssh = ssh.clone();
                let credential_store = Arc::clone(&credential_store);
                let ssh_host_vessels = ssh_host_vessels.clone();
                async move {
                    if let Err(error) =
                        apply_ssh_host_observation(&daemon, &namespace, &ssh, Some(&credential_store), ssh_host_vessels.as_deref()).await
                    {
                        warn!(host = %ssh.provisioning.host_id, %error, "failed to publish SSH host observation");
                    }
                    // #3792: a removed vessel's agent goes with it, including
                    // one removed while this daemon was down.
                    if let Some(vessels) = &ssh_host_vessels {
                        let retired = async {
                            let live =
                                ssh_host_vessels::live_vessels(&daemon.resource_backend(), &namespace, ssh.environment_id.as_str()).await?;
                            vessels.retire_absent(&ssh, &live).await
                        }
                        .await;
                        if let Err(error) = retired {
                            warn!(host = %ssh.provisioning.host_id, %error, "failed to retire removed vessels' agents");
                        }
                    }
                }
            }));
        }
        let desired_manifest_root = manifests
            .as_ref()
            .filter(|declared| manifest_reconciler_enabled(&declared.reconciler_root, &profile.host_id))
            .map(|declared| manifest_root_name(&declared.reconciler_root, &declared.dir, &declared.source));
        let roots = daemon.manifest_roots();
        for root in retry_phase("list_manifest_roots", || roots.roots(&options.namespace)).await? {
            if root.local
                && root.name.starts_with("manifest-")
                && root.spec.host == profile.host_id
                && desired_manifest_root.as_deref() != Some(root.name.as_str())
            {
                retry_phase("retire_stale_manifest_root", || roots.retire(&options.namespace, &root.name)).await?;
            }
        }
        let charter_daemon = Arc::clone(&daemon);
        tasks.push(spawn_periodic_task(MANIFEST_RECONCILE_INTERVAL, PeriodicTaskStart::Immediate, move || {
            let daemon = Arc::clone(&charter_daemon);
            async move {
                if let Err(error) = daemon.reconcile_bound_project_charters().await {
                    warn!(%error, "bound project charter reconciliation failed");
                }
            }
        }));
        if let Some(manifests) = manifests.clone() {
            if manifest_reconciler_enabled(&manifests.reconciler_root, &profile.host_id) {
                tasks.push(spawn_manifest_reconciler_task(
                    Arc::clone(&daemon),
                    options.namespace.clone(),
                    manifests,
                    MANIFEST_RECONCILE_INTERVAL,
                    options.controller_supervision.clone(),
                    runtime_health.clone(),
                ));
            } else {
                warn!(
                    source = %manifests.source,
                    declared_root = %manifests.reconciler_root,
                    local_root = %profile.host_id,
                    "manifest source belongs to another root; reconciliation disabled"
                );
            }
        }

        let controller_metrics = Arc::new(flotilla_store::load_metrics::ControllerMetricsRegistry::default());
        let mut controller_state = None;
        daemon
            .install_convoy_ensure_reconciler(Arc::new(
                EnsureReconciler::builder()
                    .teardown(daemon.convoy_teardown())
                    .resource_backend(daemon.resource_backend())
                    .clock(daemon.clock_for_scenarios())
                    .manifest_roots(daemon.manifest_roots())
                    .build(),
            ))
            .await;
        if options.start_controllers {
            // ADR 0062: composed images are built by this host's build
            // service (the Docker provider's owned BuildKit builder) when a
            // home demands a build of it; the ImageBuild controller enforces
            // Host.spec.image_build_capacity, including single-host local
            // mode's undeclared default. This host also publishes its
            // cache inventory for placement cost.
            let image_build_service = local_registry
                .environment_providers
                .for_legacy_input(flotilla_environment::environment::EnvironmentInput::Docker)
                .and_then(|(_, provider)| provider.image_build_service(owned_builder_settings(config.state_dir().as_path())));
            let image_inventory = match local_registry
                .environment_providers
                .for_legacy_input(flotilla_environment::environment::EnvironmentInput::Docker)
                .filter(|(_, provider)| provider.local_image_cache().is_some())
            {
                Some((_, provider)) => Some(Arc::new(
                    flotilla_provisioning::image_inventory::ImageInventoryPublisher::builder()
                        .io(Arc::new(ports::ProviderImageIo::builder().provider(Arc::clone(provider)).build()))
                        .backend(daemon.resource_backend())
                        .namespace(options.namespace.clone())
                        .host(profile.host_id.clone())
                        .provider_instance(
                            local_registry
                                .environment_providers
                                .instance_name(provider)
                                .ok_or("image cache provider instance is not registered")?
                                .to_string(),
                        )
                        .registered_instances(registered_image_caches.clone())
                        .build(),
                )),
                // Host-direct has no local image cache. An ambiguous provider
                // registry also cannot attest which instance owns an observation.
                None => None,
            };
            let state = Arc::new(
                ControllerRuntimeState::new(
                    Arc::clone(&daemon),
                    Arc::clone(&config),
                    Arc::clone(&local_registry),
                    daemon_socket_path.map(DaemonHostPath::new),
                    profile.host_id.clone(),
                    profile.host_direct_environment_name(),
                )
                .with_environment_sockets(options.environment_sockets.clone())
                .with_ssh_host_vessels(ssh_host_vessels.clone())
                .with_hook_endpoint(options.hook_endpoint.clone())
                .with_checkout_removal_concurrency(daemon_config.checkout_removal_concurrency)
                .with_namespace(options.namespace.clone())
                .with_ssh_host(ssh_profiles.clone())
                .with_credential_store(Arc::clone(&credential_store))
                .with_agent_material(agent_material)
                .with_blob_store(Arc::clone(&blob_store))
                .with_image_inventory(image_inventory)
                .with_image_build_service(image_build_service)
                .with_forge_default_branches(forge_default_branches)
                .with_controller_metrics(Arc::clone(&controller_metrics))
                .with_runtime_health(runtime_health.clone()),
            );
            if state.ssh_host_vessels.is_some() {
                daemon.set_ssh_host_vessel_access(Arc::new(ssh_host_vessels::VesselSessions { state: Arc::downgrade(&state) }));
            }
            phase(
                "set_operator_reconciler",
                daemon.set_operator_reconciler(Arc::new(RuntimeOperatorReconciler {
                    state: Arc::clone(&state),
                    manifests,
                    local_root: profile.host_id.clone(),
                })),
            )
            .await;
            phase(
                "set_work_credential_reconciler",
                daemon.set_work_credential_reconciler(Arc::new(RuntimeWorkCredentialReconciler { state: Arc::downgrade(&state) })),
            )
            .await;
            daemon.set_session_capability_source(Arc::new(RuntimeSessionCapabilities { state: Arc::downgrade(&state) })).await;
            controller_state = Some(state);
        }

        daemon.begin_environment_adoption();
        tasks.push(spawn_startup_restoration(
            StartupRestoration::builder()
                .daemon(daemon)
                .config(config)
                .local_registry(local_registry)
                .credential_store(credential_store)
                .maybe_state(controller_state)
                .provisioning(provisioning)
                .options(options)
                .runtime_health(runtime_health)
                .build(),
        ));

        Ok(Self { tasks, blob_store, controller_metrics, stop_expected: false })
    }
}

/// Owns deferred recovery and the controllers whose first pass depends on it.
#[derive(bon::Builder)]
struct StartupRestoration {
    daemon: Arc<InProcessDaemon>,
    config: Arc<ConfigStore>,
    local_registry: Arc<ProviderRegistry>,
    credential_store: Arc<CredentialStore>,
    state: Option<Arc<ControllerRuntimeState>>,
    /// The provisioning kinds' engine homes, the single writer of the resources they hold, for
    /// their controllers.
    provisioning: ProvisioningLifecycles,
    options: RuntimeOptions,
    runtime_health: RuntimeHealth,
    /// The credential delivery controller, once its start pass has run.
    #[builder(skip)]
    credential_delivery: std::sync::Mutex<Option<CredentialDelivery>>,
}

impl StartupRestoration {
    async fn restore(&self) {
        let daemon = &self.daemon;
        let config = &self.config;
        let local_registry = &self.local_registry;
        let credential_store = &self.credential_store;
        let options = &self.options;
        if let Some((_, provider)) =
            local_registry.environment_providers.for_legacy_input(flotilla_environment::environment::EnvironmentInput::Docker)
        {
            let reads = daemon.lifecycle_reads();
            let live = phase("list_environments_for_credential_sweep", tokio::task::spawn_blocking(move || reads.environments()?.live()))
                .await
                .map_err(|error| error.to_string())
                .and_then(|live| live);
            let running = phase("list_docker_backings_for_credential_sweep", provider.list()).await;
            match (live, running) {
                (Ok(live), Ok(running)) => {
                    let live = live.into_iter().map(|environment| environment.name).collect::<BTreeSet<_>>();
                    let running = running.into_iter().map(|handle| handle.id().to_string()).collect::<BTreeSet<_>>();
                    if let Err(error) =
                        phase("sweep_orphaned_registry_configs", credential_store.sweep_orphaned_registry_configs(&live, &running)).await
                    {
                        warn!(%error, "failed to sweep orphaned Docker credential caches");
                    }
                }
                (Err(error), _) => warn!(%error, "could not list environments for Docker credential cache sweep"),
                (_, Err(error)) => warn!(%error, "could not list running Docker backings for credential cache sweep"),
            }
        } else if config.state_dir().as_path().join("credential-runtime").exists() {
            warn!("Docker credential cache sweep deferred because Docker backing liveness is unavailable");
        }
        let Some(state) = self.state.as_ref() else {
            return;
        };
        if let Err(error) = phase("reconcile_provisioned_environments", reconcile_provisioned_environments(state, &options.namespace)).await
        {
            warn!(%error, "failed to restore provisioned environments during startup; periodic reconciliation will retry");
        }
        if let Some(mut delivery) = CredentialDelivery::new(Arc::clone(state)) {
            if let Err(error) = phase("reconcile_work_credentials", delivery.start()).await {
                warn!(%error, "failed to reconcile work credentials during startup; each environment is retried at its backoff");
            }
            *self.credential_delivery.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(delivery);
        }
    }

    async fn run(&self) -> Result<(), ResourceError> {
        // Preserve adoption -> credentials -> resource-controller ordering.
        // Dispatch also waits so new work does not compete with initial recovery.
        // Health and clients remain available while restoration is pending; the
        // initial pass deliberately has no deadline: starting controllers after
        // partial adoption could violate ownership and credential-staging order.
        // Ordinary failures still let periodic reconciliation retry.
        self.restore().await;
        self.daemon.finish_environment_adoption();
        let daemon = &self.daemon;
        let options = &self.options;
        let runtime_health = &self.runtime_health;
        let mut controller_tasks = Vec::new();
        let observation_daemon = Arc::clone(daemon);
        let observation_namespace = options.namespace.clone();
        controller_tasks.push(AbortOnDropHandle::new(tokio::spawn(async move {
            observation_daemon.watch_change_request_convoy_demands(&observation_namespace).await;
        })));
        let Some(state) = self.state.as_ref() else {
            return futures::future::pending::<Result<(), ResourceError>>().await;
        };
        controller_tasks.push(AbortOnDropHandle::new(spawn_dispatch_reconciler_task(Arc::clone(daemon), options.namespace.clone())));
        controller_tasks.push(AbortOnDropHandle::new(spawn_convoy_ensure_reconciler_task(
            Arc::clone(state),
            options.namespace.clone(),
            options.controller_resync_interval,
            runtime_health.clone(),
        )));
        let (supervisor_turn_task, _supervisor_turn_ready) =
            spawn_pending_supervisor_turn_task(Arc::clone(daemon), options.namespace.clone());
        controller_tasks.push(AbortOnDropHandle::new(supervisor_turn_task));
        controller_tasks.push(AbortOnDropHandle::new(spawn_provisioned_environment_reconciliation_task(
            Arc::clone(state),
            options.namespace.clone(),
            options.controller_resync_interval,
        )));
        if let Some(delivery) = self.credential_delivery.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take() {
            controller_tasks
                .push(AbortOnDropHandle::new(tokio::spawn(delivery.run(state.controller_metrics.for_kind("CredentialDelivery")))));
        }
        // The central Codex login's sole writer; each rotation wakes credential delivery.
        let env = Arc::clone(&state.daemon.discovery_runtime().env);
        let events = Arc::clone(&state.credential_events);
        controller_tasks.push(AbortOnDropHandle::new(tokio::spawn(
            CodexCentralRefresher::new(codex_central_auth_path(&*env), &*env).run(move || events.codex_rotated()),
        )));
        controller_tasks.push(AbortOnDropHandle::new(spawn_environment_orphan_sweep_task(Arc::clone(state))));
        controller_tasks.push(AbortOnDropHandle::new(remotes::spawn_remote_probe_task(Arc::clone(state), remotes::REMOTE_PROBE_INTERVAL)));
        controller_tasks.extend(
            spawn_controller_loops(
                Arc::clone(state),
                &self.provisioning,
                &options.namespace,
                options.controller_supervision.clone(),
                runtime_health.clone(),
            )
            .into_iter()
            .map(AbortOnDropHandle::new),
        );
        // The guards abort all owned controllers when shutdown cancels restoration.
        futures::future::pending::<Result<(), ResourceError>>().await
    }
}

impl DaemonRuntime {
    /// Stop the supervisory tasks deliberately. Every routine daemon exit —
    /// SIGTERM, SIGINT, idle timeout, explicit shutdown — should call this, so
    /// that `Drop`'s ERROR path stays reserved for a runtime that disappeared
    /// while the daemon was still meant to be working.
    pub fn shutdown(mut self) {
        self.stop_expected = true;
        info!(tasks = self.tasks.len(), "daemon runtime stopping; aborting supervisory tasks");
    }
}

impl Drop for DaemonRuntime {
    fn drop(&mut self) {
        // Dropping this value aborts every supervisory task — heartbeat, replica
        // refresh, aggregator, controllers. A daemon whose runtime is dropped
        // while its accept loop keeps running looks alive (process up, socket
        // accepting) but serves nothing, and until this log existed it left no
        // trace whatsoever (flotilla#1111). An expected stop goes through
        // `shutdown` and logs at INFO there; reaching here without that flag is
        // the case worth shouting about.
        if self.stop_expected {
            debug!(tasks = self.tasks.len(), "daemon runtime dropped after expected shutdown");
        } else {
            error!(
                tasks = self.tasks.len(),
                "daemon runtime dropped unexpectedly; aborting supervisory tasks — the daemon can no longer do background work"
            );
        }
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn spawn_controller_loops(
    state: Arc<ControllerRuntimeState>,
    provisioning: &ProvisioningLifecycles,
    namespace: &str,
    supervision: ControllerSupervision,
    runtime_health: RuntimeHealth,
) -> Vec<JoinHandle<()>> {
    let backend = state.daemon.resource_backend();
    let status_lifecycle_reads = state.daemon.lifecycle_reads();
    let gh_default_branches = state
        .daemon
        .local_command_runner()
        .map(|runner| Arc::new(GhForgeDefaultBranchResolver::new(runner)) as Arc<dyn ForgeDefaultBranchResolver>);
    let forge_default_branch_resolver = match (state.forge_default_branches.clone(), gh_default_branches) {
        (Some(first), Some(then)) => {
            Some(Arc::new(ChainedForgeDefaultBranchResolver(vec![first, then])) as Arc<dyn ForgeDefaultBranchResolver>)
        }
        (first, then) => first.or(then),
    };
    let namespace_string = namespace.to_string();

    macro_rules! controller {
        ($primary:ty, $make_parts:expr) => {
            controller!($primary, <$primary>::API_PATHS.kind, $make_parts)
        };
        ($primary:ty, $name:expr, $make_parts:expr) => {{
            let make_parts = $make_parts;
            let write_lifecycle_reads = Arc::clone(&status_lifecycle_reads);
            let loop_metrics = Arc::clone(&state.controller_metrics);
            spawn_named_controller(
                $name,
                backend.clone(),
                namespace_string.clone(),
                supervision.clone(),
                runtime_health.clone(),
                move |backend, namespace| {
                    let (secondaries, reconciler) = make_parts(backend.clone(), namespace.clone());
                    let controller = ControllerLoop {
                        primary: backend
                            .clone()
                            .using::<$primary>(&namespace)
                            .with_lifecycle_status_reads(Arc::clone(&write_lifecycle_reads)),
                        secondaries,
                        reconciler,
                        backend,
                    };
                    let metrics = loop_metrics.for_kind($name);
                    async move { controller.run_observed(metrics).await }
                },
            )
        }};
    }

    let mut controllers = vec![
        spawn_vessel_requests(
            flotilla_admission::reconcilers::vessel_requests::VesselRequests::new(
                std::sync::Arc::new(flotilla_admission::reconcilers::vessel_requests::OldConvoyWorks::new(
                    backend.clone(),
                    namespace_string.clone(),
                )),
                state.daemon.lifecycle_reads(),
                Arc::new(ports::DaemonVesselReleaseGate { daemon: Arc::clone(&state.daemon), namespace: namespace_string.clone() }),
                CanonicalHostId::resolved(state.local_host_ref.clone()),
                state.ssh_host_refs(),
            ),
            supervision.clone(),
            runtime_health.clone(),
        ),
        controller!(Repository, {
            let lifecycle_reads = state.daemon.lifecycle_reads();
            let discovered = state.daemon.discovered_checkouts();
            let forge_default_branch_resolver = forge_default_branch_resolver.clone();
            // A repository's clones wake it as their launches change.
            let decided = state.daemon.decided_store().ok();
            move |_: ResourceBackend, _: String| {
                let forge_default_branch_resolver = forge_default_branch_resolver.clone();
                let mut reconciler = RepositoryReconciler::new(discovered.clone(), lifecycle_reads.clone());
                if let Some(resolver) = forge_default_branch_resolver {
                    reconciler = reconciler.with_forge_default_branch_resolver(resolver);
                }
                (RepositoryReconciler::secondary_watches(discovered.clone(), decided.clone()), reconciler)
            }
        }),
        controller!(Vessel, {
            let state = Arc::clone(&state);
            move |backend: ResourceBackend, namespace_string: String| {
                // A ready vessel's crews are seated as it, its sessions, its convoy, its crews'
                // bindings and their briefs change; the Vessel controller writes its edges.
                let mut secondaries = CrewSeats::secondary_watches();
                secondaries.push(Box::new(ConvoyVesselWake::new(
                    &backend,
                    &namespace_string,
                    state.daemon.lifecycle_reads(),
                    state.daemon.convoy_engine_path(),
                    state.daemon.account_records(),
                )));
                let planning = Arc::new(vessel_planning(&state, &backend, &namespace_string));
                (secondaries, CrewSeats::new(planning).with_rechecks(state.vessel_rechecks.clone()))
            }
        }),
        controller!(Convoy, {
            let daemon = Arc::clone(&state.daemon);
            move |backend: ResourceBackend, namespace_string: String| {
                let daemon = Arc::clone(&daemon);
                let mut secondaries = ConvoyReconciler::secondary_watches(
                    &backend,
                    &namespace_string,
                    daemon.convoy_engine_path(),
                    daemon.crew_message_path(),
                );
                secondaries.push(daemon.reconciler_wake_watch());
                secondaries.push(Box::new(flotilla_crew::convoy::EngineConvoyWake { engine: daemon.convoy_engine_path() }));
                // A convoy's checkouts are the engine's: their changes wake the convoy holding them.
                if let Ok(service) = daemon.decided_store() {
                    secondaries.push(Box::new(flotilla_provisioning::engine::checkouts::CheckoutConvoyWake::new(Arc::clone(&service))));
                    // A convoy reads its vessels' phases from the engine, so their records wake it.
                    secondaries.push(Box::new(flotilla_provisioning::engine::vessel::VesselConvoyWake::new(service)));
                }
                (
                    secondaries,
                    ConvoyReconciler::new(daemon.lifecycle_reads())
                        .with_engine(daemon.convoy_engine_path())
                        .with_messages(daemon.crew_message_path())
                        .with_hosts(backend.including_replicas::<Host>(&namespace_string))
                        .with_convoys(backend.including_replicas::<Convoy>(&namespace_string))
                        .with_vessels(backend.including_replicas::<Vessel>(&namespace_string))
                        .with_forges(backend.definitions::<Forge>(&namespace_string))
                        .with_change_requests(
                            backend.including_replicas::<flotilla_resources::ChangeRequest>(&namespace_string),
                            daemon.change_request_stale_after(),
                        )
                        .with_artifacts(backend.including_replicas::<flotilla_resources::Artifact>(&namespace_string))
                        .with_landing_evidence_stale_after(LANDING_EVIDENCE_TTL)
                        .with_prepared_snapshot_gc(flotilla_store::PreparedSnapshotGarbageCollector::new(
                            backend.clone(),
                            &namespace_string,
                        )),
                )
            }
        }),
    ];
    controllers.extend(spawn_environment_controller(&state, provisioning));
    controllers.extend(spawn_clone_controller(&state, provisioning));
    let checkout = spawn_checkout_controller(&state, provisioning, &mut controllers);
    controllers.extend(checkout);
    let terminal = spawn_terminal_controller(&state, provisioning, &mut controllers);
    controllers.extend(terminal);
    let vessel = spawn_vessel_controller(&state, provisioning, &mut controllers);
    controllers.extend(vessel);
    if let Some(distributor) = &state.image_inventory {
        let distributor = Arc::clone(distributor);
        controllers.push(tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                if let Err(reason) = distributor.refresh().await {
                    tracing::warn!(%reason, "image availability refresh failed");
                }
            }
        }));
    }
    controllers.extend(spawn_image_build_controller(&state, provisioning));
    // Message delivery is the engine path's adapter (#3317, #3108); the runtime opens that path at
    // startup, before any controller runs.
    match state.daemon.message_primitives() {
        Some(primitives) => {
            let delivery =
                self::primitives::MessageDelivery::new(Arc::clone(&state), primitives, namespace_string.clone(), runtime_health.clone());
            controllers.push(tokio::spawn(delivery.run(state.controller_metrics.for_kind("Message"))));
        }
        None => tracing::warn!(namespace = %namespace_string, "the Message path is not open; nothing delivers messages"),
    }
    // #3907, #3929: bindings per convoy on its home, provisions per place.
    controllers.push(controller!(Convoy, "AccountBinding", {
        let daemon = Arc::clone(&state.daemon);
        move |_backend: ResourceBackend, namespace: String| account_access::binding_controller(Arc::clone(&daemon), namespace)
    }));
    controllers.push({
        let state = Arc::clone(&state);
        spawn_named_controller(
            "AccountProvision",
            backend.clone(),
            namespace_string.clone(),
            supervision.clone(),
            runtime_health.clone(),
            move |_backend, namespace| account_access::run_provision_controller(Arc::clone(&state), namespace),
        )
    });
    // #3964: each access's delivery status, removed with its place or convoy.
    controllers.push(controller!(flotilla_resources::AccessDelivery, {
        let state = Arc::clone(&state);
        move |_backend: ResourceBackend, namespace: String| access_layer::status::status_controller(Arc::clone(&state), namespace)
    }));
    controllers
}

/// The decided store's kinds ([`flotilla_core::in_process::decided::registry`]).
pub(crate) fn decided_registry() -> Result<flotilla_schema::KindRegistry, flotilla_schema::RegistryError> {
    flotilla_core::in_process::decided::registry()
}

/// The machine diagram of each kind in [`decided_registry`], by kind name.
pub(crate) fn decided_diagrams() -> Result<std::collections::BTreeMap<String, String>, flotilla_schema::RegistryError> {
    flotilla_core::in_process::decided::diagrams()
}

#[cfg(test)]
mod decided_kinds_tests {
    // Behaviour (#3629 layers 2 and 6, #3824 layer 1): behind `[primitives]` the decided store
    // holds the provisioning lifecycle kinds, the crew kinds and the account kinds beside Message
    // and AgentSession, and every kind with a machine has its diagram for the table views.
    #[test]
    fn the_decided_store_holds_the_provisioning_kinds_with_their_diagrams() {
        let registry = super::decided_registry().expect("no code is held twice");
        let kinds: Vec<&str> = registry.iter().map(|set| set.kind.as_str()).collect();
        for kind in ["Message", "AgentSession", "Environment", "ImageBuild", "Clone", "Checkout", "TerminalSession", "Vessel"] {
            assert!(kinds.contains(&kind), "{kind} in {kinds:?}");
        }
        assert!(kinds.contains(&"AccountBinding") && kinds.contains(&"AccountProvision"), "{kinds:?}");
        // A plain kind (#3824 layer 0) declares no edges and has no machine to draw.
        let diagrams = super::decided_diagrams().expect("diagrams");
        for set in registry.iter() {
            assert_eq!(diagrams.contains_key(&set.kind), !set.edges.is_empty(), "{}", set.kind);
        }
        assert!(diagrams.values().all(|diagram| diagram.contains("stateDiagram")));
    }
}

/// The decided store's opener (#3108), for the composition root to pass to
/// [`InProcessDaemon::new_composed`]. Once the daemon's identity is resolved it fills the store
/// from the old one at its first start ([`crate::engine_import::first_start`]), before this
/// daemon serves or connects to a peer, so an importing host neither sends nor receives records;
/// then it opens the store ([`flotilla_core::in_process::decided::open`]).
pub fn engine_opener(config: &ConfigStore) -> flotilla_core::in_process::EngineOpener {
    flotilla_core::in_process::decided::opener_with(
        config.state_dir().as_path(),
        Box::new(|state_dir, home, own_host| crate::engine_import::first_start(state_dir, home, Some(own_host)).map(|_| ())),
    )
}

/// Opens the Message path over the decided store composed with the daemon (#3317). Installed
/// before peer tasks start, so every peer's log is followed from the first connection.
pub(crate) async fn open_message_primitives(daemon: &Arc<InProcessDaemon>) -> Result<(), String> {
    retry_phase("restore_message_primitives", || async {
        let daemon = Arc::clone(daemon);
        tokio::task::spawn_blocking(move || daemon.open_message_primitives()).await.map_err(|error| error.to_string())?
    })
    .await
    .map(|_| ())
}

/// The archive shipper over the decided store (#3939), uploading to every configured fleet blob
/// store. None without `[primitives]`, whose store it archives, or without a store to ship to.
fn archive_shipper(
    daemon: &Arc<InProcessDaemon>,
    stores: &[flotilla_config::BlobStoreConfig],
    health: &RuntimeHealth,
) -> Result<Option<(archive::ArchiveShipper, Arc<flotilla_schema_sqlite::stream::Service>)>, String> {
    let Ok(decided) = daemon.decided_store() else { return Ok(None) };
    if stores.is_empty() {
        info!("no [[blob_stores]] configured: ended engine resources stay local");
        return Ok(None);
    }
    let targets = archive::targets(stores)?;
    let health = health.clone();
    let shipper = archive::ArchiveShipper::new(
        decided.store(),
        archive::archive_kinds(),
        targets,
        daemon.clock_for_scenarios(),
        Arc::new(move |condition| health.report_archive(condition)),
    );
    Ok(Some((shipper, decided)))
}

/// The Environment controller (#3108): the single writer of this home's environment launches,
/// realising them through the host's providers and told by its probes what became of them.
fn spawn_environment_controller(state: &Arc<ControllerRuntimeState>, engine: &ProvisioningLifecycles) -> Option<JoinHandle<()>> {
    use flotilla_provisioning::engine::{controller::Controller, environment_actuator::EnvironmentActuator, reads::StoreReads};
    let backings = state.environment_backing_events.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take()?;
    let service = match state.daemon.decided_store() {
        Ok(service) => service,
        Err(error) => {
            error!(%error, "the Environment controller has no decided store; environments will not launch");
            return None;
        }
    };
    let home = service.home().to_owned();
    let actuator = EnvironmentActuator::new(
        Arc::new(DockerControllerRuntime { state: Arc::clone(state) }),
        StoreReads::new(service.store()),
        home,
        engine.environment_facts.clone(),
        Arc::new(VesselLaunchEnvironment { backend: state.daemon.resource_backend(), namespace: state.namespace.clone() }),
    );
    match Controller::start(Arc::clone(&engine.environments), Arc::new(actuator), state.daemon.clock_for_scenarios(), service) {
        Ok(controller) => Some(tokio::spawn(controller.run(backings))),
        Err(error) => {
            error!(%error, "the Environment controller could not start; environments will not launch");
            None
        }
    }
}

/// The Clone controller (#3108): the single writer of this home's clone launches, cloning each
/// repository once into its environment. A clone that cannot reach its remote waits for this
/// host's probe (`remotes`) to reach it again.
fn spawn_clone_controller(state: &Arc<ControllerRuntimeState>, engine: &ProvisioningLifecycles) -> Option<JoinHandle<()>> {
    use flotilla_provisioning::engine::{clone_actuator::CloneActuator, controller::Controller, reads::StoreReads};
    let service = match state.daemon.decided_store() {
        Ok(service) => service,
        Err(error) => {
            error!(%error, "the Clone controller has no decided store; clones will not launch");
            return None;
        }
    };
    let home = service.home().to_owned();
    let actuator = CloneActuator::new(
        Arc::new(RoutingCloneRuntime { state: Arc::clone(state) }),
        StoreReads::new(service.store()),
        home,
        engine.clone_facts.clone(),
        Arc::clone(&state.remote_reachability) as _,
    );
    match Controller::start(Arc::clone(&engine.clones), Arc::new(actuator), state.daemon.clock_for_scenarios(), service) {
        Ok(controller) => Some(tokio::spawn(async move {
            // No probe observes a clone's backing: what it waits on is in the log.
            let (_unobserved, backings) = tokio::sync::mpsc::channel(1);
            controller.run(backings).await
        })),
        Err(error) => {
            error!(%error, "the Clone controller could not start; clones will not launch");
            None
        }
    }
}

/// The ImageBuild controller and launcher (#3108, ADR 0062): on a host with a build service and a
/// credential store, the launcher launches each build any home demands of this host, and the
/// controller is the single writer of those launches, building each through the build service.
/// Elsewhere nothing builds here.
fn spawn_image_build_controller(state: &Arc<ControllerRuntimeState>, engine: &ProvisioningLifecycles) -> Vec<JoinHandle<()>> {
    use flotilla_provisioning::engine::{
        controller::Controller, image_build_actuator::ImageBuildActuator, image_builds::ImageBuildLauncher,
    };
    let (Some(build_service), Some(credentials)) = (state.image_build_service.clone(), state.credential_store.clone()) else {
        if state.image_build_service.is_some() {
            warn!("no credential store; image builds demanded of this host will not run");
        }
        return Vec::new();
    };
    let service = match state.daemon.decided_store() {
        Ok(service) => service,
        Err(error) => {
            error!(%error, "the ImageBuild controller has no decided store; image builds will not run");
            return Vec::new();
        }
    };
    let blobs: Arc<dyn crate::blob_store::BlobStore> = match state.blob_store.clone() {
        Some(blobs) => blobs,
        None => Arc::new(crate::blob_store::LocalBlobStore::new(state.config.state_dir().as_path())),
    };
    let materials = Arc::new(crate::image_build::DaemonImageBuildMaterials {
        credentials,
        blobs,
        backend: state.daemon.resource_backend(),
        namespace: state.namespace.clone(),
        host: state.local_host_ref.clone(),
        secrets: image_builder_directory(state.config.state_dir().as_path()).join("secrets"),
    });
    let probe_wrapper: Arc<str> = flotilla_agent::process::with_preludes("exec \"$@\"").into();
    let host = state.local_host_ref.clone();
    let actuator = ImageBuildActuator::new(build_service, materials, Arc::clone(&service), host.clone(), probe_wrapper);
    let launcher = match ImageBuildLauncher::start(Arc::clone(&engine.image_builds), Arc::clone(&service), &host) {
        Ok(launcher) => launcher,
        Err(error) => {
            error!(%error, "the image build launcher could not start; image builds will not run");
            return Vec::new();
        }
    };
    match Controller::start(Arc::clone(&engine.image_builds), Arc::new(actuator), state.daemon.clock_for_scenarios(), service) {
        Ok(controller) => vec![
            tokio::spawn(launcher.run()),
            tokio::spawn(async move {
                // No probe observes a build's backing: the image stays in its cache.
                let (_unobserved, backings) = tokio::sync::mpsc::channel(1);
                controller.run(backings).await
            }),
        ],
        Err(error) => {
            error!(%error, "the ImageBuild controller could not start; image builds will not run");
            Vec::new()
        }
    }
}

/// The TerminalSession controller (#3108): the single writer of this home's terminal launches,
/// starting each in its environment's terminal pool and killing it once its holder has ended. This
/// host's probe looks at the running launches at an interval, pushing each one it finds ended.
fn spawn_terminal_controller(
    state: &Arc<ControllerRuntimeState>,
    engine: &ProvisioningLifecycles,
    tasks: &mut Vec<JoinHandle<()>>,
) -> Option<JoinHandle<()>> {
    use flotilla_provisioning::engine::{
        controller::Controller,
        reads::StoreReads,
        terminal_actuator::{TerminalActuator, TerminalProbe, TERMINAL_PROBE_EVERY},
    };
    let service = match state.daemon.decided_store() {
        Ok(service) => service,
        Err(error) => {
            error!(%error, "the TerminalSession controller has no decided store; terminal sessions will not launch");
            return None;
        }
    };
    let home = service.home().to_owned();
    let provider = Arc::new(TerminalControllerRuntime { state: Arc::clone(state) });
    let records = Arc::clone(&engine.terminal_records) as Arc<dyn flotilla_resources::terminal_records::TerminalRecords>;
    let actuator = TerminalActuator::new(
        Arc::clone(&provider),
        Arc::clone(&records),
        StoreReads::new(service.store()),
        home,
        engine.terminal_facts.clone(),
    );
    let briefs = actuator.awaited_briefs();
    match Controller::start(Arc::clone(&engine.terminal_sessions), Arc::new(actuator), state.daemon.clock_for_scenarios(), service) {
        Ok(controller) => {
            let (backings, backing_events) = tokio::sync::mpsc::channel(64);
            let (rechecks, recheck_events) = tokio::sync::mpsc::channel(64);
            let probe =
                TerminalProbe::new(provider, records, backings).with_progress(state.daemon.crew_progress()).with_briefs(briefs, rechecks);
            tasks.push(tokio::spawn(probe.run(TERMINAL_PROBE_EVERY)));
            Some(tokio::spawn(controller.run_with(backing_events, recheck_events)))
        }
        Err(error) => {
            error!(%error, "the TerminalSession controller could not start; terminal sessions will not launch");
            None
        }
    }
}

/// The Checkout controller (#3108): the single writer of this home's checkout launches, making
/// each in its environment for its holder Convoy and removing it once that convoy has ended. The
/// observed-checkout refresh names the launches whose paths it saw change, and each is observed
/// again: one still there has its registration protected again, one gone is recorded lost.
fn spawn_checkout_controller(
    state: &Arc<ControllerRuntimeState>,
    engine: &ProvisioningLifecycles,
    tasks: &mut Vec<JoinHandle<()>>,
) -> Option<JoinHandle<()>> {
    use flotilla_provisioning::engine::{checkout_actuator::CheckoutActuator, controller::Controller, reads::StoreReads};
    let rechecks = state.checkout_recheck_events.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take()?;
    let service = match state.daemon.decided_store() {
        Ok(service) => service,
        Err(error) => {
            error!(%error, "the Checkout controller has no decided store; checkouts will not launch");
            return None;
        }
    };
    let home = service.home().to_owned();
    let routing = RoutingCheckoutRuntime { state: Arc::clone(state), change_requests: None };
    let actuator = CheckoutActuator::new(
        Arc::new(checkouts::EngineCheckoutProvider { routing }),
        Arc::new(checkouts::ConvoyHolderInputs {
            backend: state.daemon.resource_backend(),
            namespace: state.namespace.clone(),
            lifecycle_reads: state.daemon.lifecycle_reads(),
        }),
        Arc::clone(&engine.checkout_records) as _,
        StoreReads::new(service.store()),
        home,
        engine.checkout_facts.clone(),
    )
    .with_branch_validation(Arc::new(checkouts::ForgeBranchValidation { daemon: Arc::clone(&state.daemon) }));
    let actuator = Arc::new(actuator);
    // This host's checkout inspector answers gates' inspection requests through the same
    // actuator, so an inspection is recorded as the controller records it.
    match state.daemon.message_primitives() {
        Some(primitives) => {
            let inspector = Arc::new(checkouts::CheckoutInspector { inspection: Arc::clone(&actuator) as _ });
            tasks.push(tokio::spawn(role_consumer::serve_role(Arc::clone(&primitives), inspector)));
            // Branch validation asks this host's forge observer to look its branch up.
            let observer = Arc::new(checkouts::ForgeObserver { daemon: Arc::clone(&state.daemon) });
            tasks.push(tokio::spawn(role_consumer::serve_role(primitives, observer)));
        }
        None => error!("the checkout inspector and forge observer have no Message path; requests to them here go unanswered"),
    }
    tasks.push(tokio::spawn(checkouts::recheck_on_discovered_refresh(Arc::clone(state))));
    match Controller::start(Arc::clone(&engine.checkouts), actuator, state.daemon.clock_for_scenarios(), service) {
        Ok(controller) => Some(tokio::spawn(async move {
            // A checkout's backing is observed at a recheck, not pushed.
            let (_unobserved, backings) = tokio::sync::mpsc::channel(1);
            controller.run_with(backings, rechecks).await
        })),
        Err(error) => {
            error!(%error, "the Checkout controller could not start; checkouts will not launch");
            None
        }
    }
}

/// This home's vessel planner over `backend` in `namespace`.
fn vessel_planning(state: &Arc<ControllerRuntimeState>, backend: &ResourceBackend, namespace: &str) -> VesselPlanning {
    VesselPlanning::new_with_config_dir(
        backend.clone(),
        namespace,
        state.daemon.account_records(),
        state.config.base_path().as_path().to_path_buf(),
    )
    .with_lifecycle_reads(state.daemon.lifecycle_reads())
    .with_worktree_metadata(Arc::new(RoutingCheckoutRuntime { state: Arc::clone(state), change_requests: None }))
    .with_federated_dependencies(backend, CanonicalHostId::resolved(state.local_host_ref.clone()))
    .with_additional_host_refs(state.ssh_host_refs())
}

/// The Vessel controller (#3108): the single writer of this home's vessel launches, realising each
/// in its environment once what its planner requests for it is realised. Its holdings' commits
/// wake it; its convoy's, its crews' bindings' and its seats' changes are told as rechecks.
fn spawn_vessel_controller(
    state: &Arc<ControllerRuntimeState>,
    engine: &ProvisioningLifecycles,
    tasks: &mut Vec<JoinHandle<()>>,
) -> Option<JoinHandle<()>> {
    use flotilla_provisioning::engine::{controller::Controller, vessel_actuator::VesselActuator};
    let rechecks = state.vessel_recheck_events.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take()?;
    let service = match state.daemon.decided_store() {
        Ok(service) => service,
        Err(error) => {
            error!(%error, "the Vessel controller has no decided store; vessels will not launch");
            return None;
        }
    };
    let backend = state.daemon.resource_backend();
    let planning = Arc::new(vessel_planning(state, &backend, &state.namespace));
    tasks.push(tokio::spawn(
        ConvoyVesselWake::new(
            &backend,
            &state.namespace,
            state.daemon.lifecycle_reads(),
            state.daemon.convoy_engine_path(),
            state.daemon.account_records(),
        )
        .recheck(state.namespace.clone(), state.vessel_rechecks.clone()),
    ));
    let actuator = Arc::new(VesselActuator::new(planning));
    match Controller::start(Arc::clone(&engine.vessels), actuator, state.daemon.clock_for_scenarios(), service) {
        Ok(controller) => Some(tokio::spawn(async move {
            // No probe observes a vessel: its holdings' commits and its rechecks look at it.
            let (_unobserved, backings) = tokio::sync::mpsc::channel(1);
            controller.run_with(backings, rechecks).await
        })),
        Err(error) => {
            error!(%error, "the Vessel controller could not start; vessels will not launch");
            None
        }
    }
}

/// The env each vessel's environment is launched with beyond its stored recipe: its crews' skills
/// and credential grants, from its convoy's frozen requirement.
struct VesselLaunchEnvironment {
    backend: ResourceBackend,
    namespace: String,
}

#[async_trait::async_trait]
impl flotilla_provisioning::engine::environment_actuator::LaunchEnvironment for VesselLaunchEnvironment {
    async fn launch_env(&self, vessel: &str) -> Result<std::collections::BTreeMap<String, String>, String> {
        flotilla_crew::reconcilers::vessel::vessel_launch_env(&self.backend, &self.namespace, vessel).await
    }
}

/// The provisioning kinds' engine homes over the decided store (#3629 layer 3), each restored
/// with the resources this home holds, and the task that tells environment readers (the
/// aggregator) what changed among the environments, written here or replicated.
async fn open_provisioning_lifecycles(
    daemon: &Arc<InProcessDaemon>,
) -> Result<(flotilla_provisioning::engine::ProvisioningLifecycles, Vec<JoinHandle<()>>), String> {
    use flotilla_engine_store::{open_home, open_home_with, StoreLog};
    use flotilla_provisioning::engine::{
        checkouts::EngineCheckouts,
        clones::EngineClones,
        environments::EngineEnvironments,
        image_builds::EngineImageBuilds,
        launch_log::{LaunchLog, StagedFacts},
        reads::{LifecycleReadPort, StoreReads},
        terminals::EngineTerminals,
        vessel::{EngineVessels, PlacementLifecycles},
        LifecycleHome, ProvisioningLifecycles,
    };
    let store = daemon.decided_store()?.store();
    let home = StoreLog::new(Arc::clone(&store)).home().to_owned();
    let opened = Arc::clone(&store);
    let service = daemon.decided_store()?;
    let lifecycles = tokio::task::spawn_blocking(move || {
        let store = opened;
        let home = StoreLog::new(Arc::clone(&store)).home().to_owned();
        let log = || StoreLog::new(Arc::clone(&store));
        // An environment's launch facts are written in the transaction that realises it.
        let environment_facts = StagedFacts::default();
        // A clone's facts (the remote's default branch) are written in the transaction that realises it.
        let clone_facts = StagedFacts::default();
        // A checkout's facts are written in the transaction that realises it, and each launch is
        // created under the holder Convoy the port read for it.
        let checkout_facts = StagedFacts::default();
        let checkout_inputs = StagedFacts::default();
        let checkouts: Arc<dyn flotilla_provisioning::engine::Lifecycles<_>> = Arc::new(LifecycleHome::new(
            &home,
            open_home_with(&log(), LaunchLog::new(log(), checkout_facts.clone()).with_inputs(checkout_inputs.clone()))?,
        ));
        let followed = Arc::clone(&service);
        let checkout_records =
            Arc::new(EngineCheckouts::new(Arc::clone(&checkouts), Arc::clone(&store), checkout_inputs, move || followed.homes()));
        // A terminal launch's facts are written in the transaction that realises it.
        let terminal_facts = StagedFacts::default();
        let terminal_sessions: Arc<dyn flotilla_provisioning::engine::Lifecycles<_>> =
            Arc::new(LifecycleHome::new(&home, open_home_with(&log(), LaunchLog::new(log(), terminal_facts.clone()))?));
        let terminal_records = Arc::new(EngineTerminals::over(Arc::clone(&terminal_sessions), Arc::clone(&service)));
        let image_builds: Arc<dyn flotilla_provisioning::engine::Lifecycles<_>> = Arc::new(LifecycleHome::new(&home, open_home(log())?));
        let image_build_records = Arc::new(EngineImageBuilds::over(Arc::clone(&image_builds), service));
        Ok::<_, String>(ProvisioningLifecycles {
            environments: Arc::new(LifecycleHome::new(&home, open_home_with(&log(), LaunchLog::new(log(), environment_facts.clone()))?)),
            environment_facts,
            image_builds,
            image_build_records,
            clones: Arc::new(LifecycleHome::new(&home, open_home_with(&log(), LaunchLog::new(log(), clone_facts.clone()))?)),
            clone_facts,
            checkouts,
            checkout_facts,
            checkout_records,
            terminal_sessions,
            terminal_facts,
            terminal_records,
            vessels: Arc::new(PlacementLifecycles::new(&home, open_home(log())?)?),
        })
    })
    .await
    .map_err(|error| error.to_string())??;
    // Every environment reader and requester goes through the engine's environments.
    let environments = Arc::new(EngineEnvironments::over(Arc::clone(&lifecycles.environments), daemon.decided_store()?));
    let environment_changes = environments.follow(daemon.decided_store()?)?;
    // Every clone reader and requester goes through the engine's clones.
    let clones = Arc::new(EngineClones::over(Arc::clone(&lifecycles.clones) as _, daemon.decided_store()?));
    let clone_changes = clones.follow(daemon.decided_store()?)?;
    // Every checkout reader and requester goes through the engine's checkouts.
    let checkout_changes = lifecycles.checkout_records.follow(daemon.decided_store()?)?;
    // Every terminal-session reader and requester goes through the engine's launches.
    let terminals = Arc::clone(&lifecycles.terminal_records);
    let terminal_changes = terminals.follow(daemon.decided_store()?)?;
    // Every vessel reader goes through the engine's launches; this home's are written only here.
    let vessels = Arc::new(EngineVessels::over(Arc::clone(&lifecycles.vessels), daemon.decided_store()?));
    let vessel_changes = vessels.follow(daemon.decided_store()?)?;
    daemon.install_lifecycle_reads(Arc::new(
        LifecycleReadPort::new(StoreReads::new(store), home)
            .with_environments(environments as Arc<dyn flotilla_resources::environment_records::EnvironmentRecords>)
            .with_clones(clones as Arc<dyn flotilla_resources::clone_records::CloneRecords>)
            .with_image_builds(
                Arc::clone(&lifecycles.image_build_records) as Arc<dyn flotilla_resources::image_build_records::ImageBuildRecords>
            )
            .with_checkouts(Arc::clone(&lifecycles.checkout_records) as Arc<dyn flotilla_resources::checkout_records::CheckoutRecords>)
            .with_terminals(terminals as Arc<dyn flotilla_resources::terminal_records::TerminalRecords>)
            .with_vessels(vessels as Arc<dyn flotilla_resources::vessel_records::VesselRecords>),
    ))?;
    Ok((lifecycles, vec![environment_changes, clone_changes, checkout_changes, terminal_changes, vessel_changes]))
}

/// Daemon-owned image build state: the owned builder's Buildx state, build
/// scratch and staged build secrets, all private to this daemon.
fn image_builder_directory(state_dir: &std::path::Path) -> std::path::PathBuf {
    state_dir.join("image-builder")
}

/// The owned BuildKit builder of this daemon (ADR 0062). Its Buildx state is
/// under the daemon state directory, so the instance is invisible to other
/// Buildx users on the host and survives daemon restarts.
fn owned_builder_settings(state_dir: &std::path::Path) -> flotilla_environment::environment::OwnedBuilderSettings {
    let directory = image_builder_directory(state_dir);
    flotilla_environment::environment::OwnedBuilderSettings {
        builder: OWNED_BUILDER_NAME.into(),
        state_directory: directory.join("buildx"),
        scratch: directory.join("scratch"),
    }
}

const OWNED_BUILDER_NAME: &str = "flotilla-images";

#[cfg(test)]
mod test_git_repo;
#[cfg(test)]
mod tests;

/// Optional provider-only Drydock configuration, served by drydock's daemon
/// embedded here: it supervises the hosts and keeps each vessel's key and
/// exposures under the state directory, across restarts. The adapter sees only
/// ProviderApi.
#[cfg(unix)]
async fn compose_drydock(config: &ConfigStore, registry: &mut ProviderRegistry) -> Result<(), String> {
    use flotilla_discovery_api::{ProviderCategory, ProviderDescriptor};
    use flotilla_environment::environment::{drydock::DrydockEnvironmentProvider, EnvironmentProvider};
    let path = config.base_path().as_path().join("drydock.toml");
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("read drydock provider configuration: {error}")),
    };
    let (provider_text, lab_reach) = flotilla_config::drydock::split_configuration(&text)?;
    let configuration = drydock::config::Config::parse(&provider_text)?;
    if configuration.providers.is_empty() {
        return Ok(());
    }
    let instances: Vec<(String, String)> = configuration.providers.iter().map(|(name, p)| (name.clone(), p.module.clone())).collect();
    // A missing bundled host leaves its providers advertised as missing-tool.
    let hosts = drydock::daemon::HostOption::installed().await.unwrap_or_else(|error| {
        warn!(%error, "drydock's bundled host is not installed; its providers stay missing-tool unless drydock-host is on PATH");
        vec![drydock::daemon::HostOption {
            name: "rust".into(),
            optional: true,
            connection: drydock::HostConnection::Managed { command: vec!["drydock-host".into()], runtime: None },
        }]
    });
    let state = config.state_dir().as_path().to_path_buf();
    let run = drydock_run_directory(&state);
    let seams = drydock::daemon::Seams {
        builds: Arc::new(InstalledAgentBuilds),
        ..drydock::daemon::Seams::new(Arc::new(drydock::launch::RegistryResolver::new().map_err(|error| error.to_string())?))
    };
    // Keys and exposure records are durable state; the run directory holds
    // only sockets, which drydock makes again when it starts.
    let daemon = drydock::daemon::Daemon::start_with_seams(configuration, &run, state.join("drydock").join("keys"), hosts, seams).await?;
    for (name, module) in instances {
        if registry.environment_providers.contains_key(&name) {
            return Err(format!("duplicate environment provider instance {name}"));
        }
        let adapter: Arc<dyn EnvironmentProvider> = Arc::new(
            DrydockEnvironmentProvider::new(
                flotilla_drydock::api::provider_api(daemon.clone()),
                flotilla_drydock::agent::link(),
                name.clone(),
                module,
            )
            .with_lab_reach(lab_reach.get(&name).map(|reach| (reach.hostname.clone(), reach.port))),
        );
        adapter.detect_readiness().await;
        registry.environment_providers.insert(
            name.clone(),
            ProviderDescriptor::named(ProviderCategory::EnvironmentProvider, &name),
            adapter,
        );
    }
    Ok(())
}

/// Where drydock's sockets live: the exposures' local ends and its hosts'.
/// Short and fixed below `/tmp`, as `sun_path` allows 104 bytes on macOS
/// whatever the state directory, and one per state directory.
#[cfg(unix)]
fn drydock_run_directory(state: &std::path::Path) -> PathBuf {
    use sha2::Digest;
    let digest = format!("{:x}", sha2::Sha256::digest(state.as_os_str().as_encoded_bytes()));
    PathBuf::from(format!("/tmp/flotilla-drydock-{}", &digest[..16]))
}

/// The installed generation's agent companion: the build the adapter pushes
/// too, so drydock and flotilla never upgrade a vessel's agent past each other.
#[cfg(unix)]
struct InstalledAgentBuilds;
#[cfg(unix)]
#[async_trait::async_trait]
impl drydock::daemon::AgentBuilds for InstalledAgentBuilds {
    async fn build(&self, platform: &drydock_protocol::Platform) -> Result<Vec<u8>, String> {
        let platform = flotilla_environment::platform::Platform::new(&platform.os, &platform.architecture);
        flotilla_environment::environment::drydock::installed_agent_build(&platform).await
    }
}
