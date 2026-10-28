//! Daemon composition, startup restoration, and resource-controller wiring.

mod access_layer;
mod account_access;
pub(crate) mod archive;
mod credentials;
mod discovery;
mod docker;
mod environments;
mod health;
mod message;
mod ports;
mod primitives;
mod provider_path;
mod seed;
pub(crate) mod ssh_host_vessels;
mod state;
mod tasks;
mod terminal;
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{atomic::AtomicU64, Arc},
    time::Duration,
};
pub(crate) use terminal::{DELIVERY_CONFIRMATION_POLL, DELIVERY_CONFIRMATION_STABLE_FOR};

use chrono::Utc;
use flotilla_admission::reconcilers::convoy_ensure::EnsureReconciler;
use flotilla_config::ConfigStore;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_credentials::{AgentMaterialRegistry, CredentialStore};
use flotilla_crew::convoy::ConvoyReconciler;
use flotilla_crew::reconcilers::terminal_session::TerminalSessionReconciler;
use flotilla_crew::reconcilers::vessel::VesselReconciler;
use flotilla_discovery::registry::ProviderRegistry;
use flotilla_paths::path_context::DaemonHostPath;
use flotilla_protocol::CanonicalHostId;
use flotilla_provisioning::engine::{
    checkout::CheckoutBinding, clone::CloneBinding, environment::EnvironmentBinding, image_build::ImageBuildBinding,
    terminal_session::TerminalSessionBinding, vessel::VesselBinding, Engine,
};
use flotilla_provisioning::reconcilers::{
    CheckoutReconciler, CloneReconciler, EnvironmentReconciler, ForgeDefaultBranchResolver, RepositoryReconciler,
};
use flotilla_resources::LANDING_EVIDENCE_TTL;
use flotilla_resources::{
    host_direct_environment_name, ChangeRequest, Checkout, Clone, Convoy, Environment, Forge, Host, Repository, Resource, ResourceError,
    SystemClock, TerminalSession, Vessel, WorkflowTemplate,
};
use flotilla_store::{controller::ControllerLoop, ResourceBackend};
use tokio::{sync::watch, task::JoinHandle};
use tokio_util::task::AbortOnDropHandle;
use tracing::{debug, error, info, warn};

use self::credentials::{
    reconcile_work_credentials, spawn_codex_central_refresh_task, spawn_codex_credential_redelivery_task, spawn_credential_refresh_task,
    RuntimeSessionCapabilities, RuntimeWorkCredentialReconciler,
};
use self::discovery::{
    apply_ssh_host_observation, build_local_profile, discover_ssh_host_profiles, probe_local_provider_registry, register_ssh_host_resources,
};
use self::docker::DockerControllerRuntime;
use self::environments::reconcile_provisioned_environments;
use self::health::{
    apply_host_heartbeat_with_credentials, spawn_controller_loop_watchdog, spawn_projection_parity_task, DaemonHealthIdentity,
    RuntimeHealth,
};
use self::message::MessageController;
use self::ports::{
    ChainedForgeDefaultBranchResolver, DaemonConvoyTeardownRuntime, GhForgeDefaultBranchResolver, RoutingCheckoutRuntime,
    RoutingCloneRuntime, RuntimeOperatorReconciler,
};
use self::seed::register_startup_resources;
use self::state::ControllerRuntimeState;
use self::tasks::{
    run_artifact_gc, run_checkout_archive_gc, spawn_adopted_checkout_reconciliation_task, spawn_aggregator_task,
    spawn_convoy_ensure_reconciler_task, spawn_demand_expiry_task, spawn_dispatch_reconciler_task, spawn_environment_orphan_sweep_task,
    spawn_event_expiry_task, spawn_heartbeat_task_with_credentials, spawn_host_description_projection_task, spawn_liveness_watchdog_task,
    spawn_local_fulfilment_probe_task, spawn_manifest_reconciler_task, spawn_named_controller, spawn_periodic_task,
    spawn_provisioned_environment_reconciliation_task, spawn_resource_controller, spawn_sleep_inhibitor_task,
    spawn_ssh_fulfilment_probe_task, spawn_startup_restoration, spawn_vessel_placement_projector, supervise_controller,
    CheckoutArchiveRoot, CheckoutArchiveSweep, PeriodicTaskStart, LIVENESS_WATCHDOG_INTERVAL, MANIFEST_RECONCILE_INTERVAL,
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
#[cfg(test)]
pub(crate) use tasks::wait_for_listening;
pub use tasks::{spawn_forge_read_demand_task, spawn_pending_supervisor_turn_task_with_watches};

#[derive(Debug, Clone, bon::Builder)]
pub struct RuntimeOptions {
    pub namespace: String,
    pub heartbeat_interval: Duration,
    pub controller_resync_interval: Duration,
    pub controller_supervision: ControllerSupervision,
    pub start_controllers: bool,
    pub codex_central_refresh_interval: Duration,
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
            controller_supervision: ControllerSupervision::default(),
            start_controllers: true,
            startup_ready: None,
            environment_sockets: None,
            hook_endpoint: None,
            // Codex only proactively refreshes within 5 minutes of expiry; a
            // central login's access token lives far longer than that, so a
            // conservative fixed cadence comfortably inside that window
            // (rather than tracking each token's own `exp`) is sufficient.
            codex_central_refresh_interval: Duration::from_secs(4 * 60 * 60),
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
        result_phase("initialize_runtime", Self::initialize_with_options(daemon, config, daemon_socket_path, options)).await
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
        // ADR 0064 switch-over (#3317): `[primitives]` moves Message onto the
        // engine and its store; refused without `[dummy_fleet]`. Absent, nothing changes.
        let mut provisioning_lifecycles = None;
        if daemon_config.primitives()?.is_some() {
            result_phase("open_message_primitives", open_message_primitives(&daemon, &config)).await?;
            provisioning_lifecycles = Some(open_provisioning_lifecycles(&daemon).await?);
            daemon.require_lifecycle_reads_installed()?;
        }
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
            generation: retry_phase("read_observed_generation", || async {
                daemon.observed_resource_backend().using::<Checkout>(&options.namespace).list().await
            })
            .await?
            .generation,
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
                register_ssh_host_resources(&daemon.resource_backend(), &options.namespace, &profile.host_id, &ssh).await
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

        let mut tasks = vec![
            spawn_local_fulfilment_probe_task(
                Arc::clone(&daemon),
                options.namespace.clone(),
                profile.clone(),
                Arc::clone(&local_registry),
                config.state_dir().as_path().join("probe-cwd"),
                runtime_health.host_observations.clone(),
            ),
            tokio::spawn(Arc::clone(&blob_store).run_sync()),
            tokio::spawn(run_artifact_gc(daemon.resource_backend(), options.namespace.clone(), Arc::clone(&blob_store))),
            tokio::spawn(run_checkout_archive_gc(
                daemon.resource_backend(),
                options.namespace.clone(),
                CheckoutArchiveSweep {
                    daemon: Arc::clone(&daemon),
                    catalog_path: config.state_dir().as_path().join("checkout-archive-roots.json"),
                    roots: archive_roots,
                    retention_days: daemon_config.checkout_archive_retention_days,
                },
            )),
            spawn_heartbeat_task_with_credentials(
                Arc::clone(&daemon),
                options.namespace.clone(),
                profile.clone(),
                Arc::new(Some(Arc::clone(&credential_store))),
                health.clone(),
                runtime_health.clone(),
                options.heartbeat_interval,
            ),
            spawn_credential_refresh_task(Arc::clone(&daemon), options.namespace.clone(), Arc::clone(&credential_store)),
            spawn_host_description_projection_task(Arc::clone(&daemon), options.namespace.clone(), options.heartbeat_interval),
            spawn_codex_central_refresh_task(Arc::clone(&daemon.discovery_runtime().env), options.codex_central_refresh_interval),
            spawn_demand_expiry_task(daemon.resource_backend(), options.namespace.clone(), options.heartbeat_interval),
            spawn_event_expiry_task(daemon.resource_backend(), options.namespace.clone(), options.heartbeat_interval),
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
        ];
        if let Some(shipper) = archive_shipper(&daemon, &daemon_config.blob_stores, &runtime_health)? {
            tasks.push(tokio::spawn(shipper.run(archive::PASS_INTERVAL)));
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
        // Behind `[primitives]` the engine's Ensure decides standing-convoy status (#3629 layer 7).
        let ensure_engine = match provisioning_lifecycles {
            Some(_) => {
                let store = daemon.decided_store()?.store();
                let path = tokio::task::spawn_blocking(move || {
                    flotilla_core::primitives::ensures::EngineEnsurePath::open(store, Arc::new(SystemClock))
                })
                .await
                .map_err(|error| error.to_string())??;
                Some(Arc::new(path) as Arc<dyn flotilla_admission::convoy_ensure::EngineEnsures>)
            }
            None => None,
        };
        daemon
            .install_convoy_ensure_reconciler(Arc::new(
                EnsureReconciler::builder()
                    .teardown(daemon.convoy_teardown())
                    .resource_backend(daemon.resource_backend())
                    .clock(Arc::new(SystemClock))
                    .maybe_engine(ensure_engine)
                    .manifest_roots(daemon.manifest_roots())
                    .build(),
            ))
            .await;
        if options.start_controllers {
            // ADR 0062: composed images are built by this host's build
            // service (the Docker provider's owned BuildKit builder) when the
            // ImageBuild reconciler finds builds for it; the reconciler
            // enforces Host.spec.image_build_capacity, including single-host
            // local mode's undeclared default. This host also publishes its
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
                .with_provisioning_lifecycles(provisioning_lifecycles.clone()),
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

        tasks.push(spawn_startup_restoration(
            StartupRestoration::builder()
                .daemon(daemon)
                .config(config)
                .local_registry(local_registry)
                .credential_store(credential_store)
                .maybe_state(controller_state)
                .options(options)
                .runtime_health(runtime_health)
                .build(),
        ));

        // +1 for the watchdog's own handle, pushed below, so this count matches
        // `self.tasks.len()` at drop time.
        let supervisory_tasks = tasks.len() + 1;
        tasks.push(spawn_liveness_watchdog_task(supervisory_tasks, LIVENESS_WATCHDOG_INTERVAL));

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
    options: RuntimeOptions,
    runtime_health: RuntimeHealth,
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
            let live =
                phase("list_environments_for_credential_sweep", daemon.resource_backend().using::<Environment>(&options.namespace).list())
                    .await;
            let running = phase("list_docker_backings_for_credential_sweep", provider.list()).await;
            match (live, running) {
                (Ok(live), Ok(running)) => {
                    let live = live.items.into_iter().map(|environment| environment.metadata.name).collect::<BTreeSet<_>>();
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
        if let Err(error) = phase("reconcile_adopted_checkouts", daemon.reconcile_adopted_checkouts(&options.namespace)).await {
            warn!(%error, "failed to restore adopted checkout observations during startup; periodic reconciliation will retry");
        }
        let Some(state) = self.state.as_ref() else {
            return;
        };
        if let Err(error) = phase("reconcile_provisioned_environments", reconcile_provisioned_environments(state, &options.namespace)).await
        {
            warn!(%error, "failed to restore provisioned environments during startup; periodic reconciliation will retry");
        }
        if let Err(error) = phase("reconcile_work_credentials", reconcile_work_credentials(state, &options.namespace)).await {
            warn!(%error, "failed to reconcile work credentials during startup; periodic reconciliation will retry");
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
        let daemon = &self.daemon;
        let options = &self.options;
        let runtime_health = &self.runtime_health;
        let mut controller_tasks = vec![AbortOnDropHandle::new(spawn_adopted_checkout_reconciliation_task(
            Arc::clone(daemon),
            options.namespace.clone(),
            options.controller_resync_interval,
        ))];
        let (forge_demands, _) = spawn_forge_read_demand_task(Arc::clone(daemon), options.namespace.clone());
        controller_tasks.push(AbortOnDropHandle::new(forge_demands));
        let observation_daemon = Arc::clone(daemon);
        let observation_namespace = options.namespace.clone();
        controller_tasks.push(AbortOnDropHandle::new(tokio::spawn(async move {
            observation_daemon.watch_change_request_convoy_demands(&observation_namespace).await;
        })));
        let Some(state) = self.state.as_ref() else {
            return futures::future::pending::<Result<(), ResourceError>>().await;
        };
        // Warm all board sources independently of readiness reconciliation and
        // interactive request cancellation. Reads schedule coalesced refreshes.
        let board_daemon = Arc::clone(daemon);
        controller_tasks.push(AbortOnDropHandle::new(spawn_periodic_task(
            options.controller_resync_interval,
            PeriodicTaskStart::Immediate,
            move || {
                let daemon = Arc::clone(&board_daemon);
                async move {
                    if let Err(error) = daemon.refresh_dispatch_boards_internal().await {
                        tracing::debug!(%error, "dispatch board observation unavailable");
                    }
                }
            },
        )));
        let audit_daemon = Arc::clone(daemon);
        // The old store's audit sweep compacts old Messages; on the engine path there are none.
        if daemon.message_primitives().is_none() {
            controller_tasks.push(AbortOnDropHandle::new(spawn_periodic_task(
                Duration::from_secs(60 * 60),
                PeriodicTaskStart::Immediate,
                move || {
                    let daemon = Arc::clone(&audit_daemon);
                    async move {
                        match daemon.resource_backend().local_namespaces::<flotilla_resources::Message>().await {
                            Ok(namespaces) => {
                                for namespace in namespaces {
                                    if let Err(error) = daemon.message_inbox(&namespace).await.compact_audit(Utc::now()).await {
                                        warn!(%error, %namespace, "Message audit retention sweep failed");
                                    }
                                }
                            }
                            Err(error) => warn!(%error, "Message retention namespace inventory failed"),
                        }
                    }
                },
            )));
        }
        if let (Some(primitives), Some(state)) = (daemon.message_primitives(), self.state.as_ref()) {
            let delivery = self::primitives::MessageDelivery::new(Arc::clone(state), primitives, options.namespace.clone());
            let interval = options.controller_resync_interval.min(Duration::from_secs(1));
            controller_tasks.push(AbortOnDropHandle::new(tokio::spawn(async move {
                if let Err(error) = delivery.run(interval).await {
                    warn!(%error, "engine message delivery stopped");
                }
            })));
        }
        controller_tasks.push(AbortOnDropHandle::new(spawn_dispatch_reconciler_task(
            Arc::clone(daemon),
            options.namespace.clone(),
            options.controller_resync_interval,
        )));
        controller_tasks.push(AbortOnDropHandle::new(spawn_convoy_ensure_reconciler_task(
            Arc::clone(state),
            options.namespace.clone(),
            options.controller_resync_interval,
            runtime_health.clone(),
        )));
        let (supervisor_turn_task, _supervisor_turn_ready) =
            spawn_pending_supervisor_turn_task(Arc::clone(daemon), options.namespace.clone(), options.controller_resync_interval);
        controller_tasks.push(AbortOnDropHandle::new(supervisor_turn_task));
        controller_tasks.push(AbortOnDropHandle::new(spawn_provisioned_environment_reconciliation_task(
            Arc::clone(state),
            options.namespace.clone(),
            options.controller_resync_interval,
        )));
        controller_tasks.push(AbortOnDropHandle::new(spawn_environment_orphan_sweep_task(Arc::clone(state))));
        controller_tasks.push(AbortOnDropHandle::new(spawn_codex_credential_redelivery_task(
            Arc::clone(state),
            options.namespace.clone(),
            options.controller_resync_interval,
        )));
        controller_tasks.extend(
            spawn_controller_loops(
                Arc::clone(state),
                &options.namespace,
                options.controller_resync_interval,
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
    namespace: &str,
    controller_resync_interval: Duration,
    supervision: ControllerSupervision,
    runtime_health: RuntimeHealth,
) -> Vec<JoinHandle<()>> {
    let backend = state.daemon.resource_backend();
    let status_lifecycle_reads = state.daemon.lifecycle_reads();
    let observed_backend = state.daemon.observed_resource_backend();
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
            let loop_health = runtime_health.clone();
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
                        resync_interval: controller_resync_interval,
                        backend,
                    };
                    let heartbeat = Arc::new(AtomicU64::new(0));
                    let health = loop_health.clone();
                    let metrics = loop_metrics.for_kind($name);
                    async move {
                        let watchdog =
                            spawn_controller_loop_watchdog($name, Arc::clone(&heartbeat), controller_resync_interval, health.clone());
                        let result = controller.run_observed(heartbeat, metrics).await;
                        watchdog.abort();
                        health.clear_controller_loop_stall($name);
                        result
                    }
                },
            )
        }};
    }

    let gc_backend = backend.clone();
    let gc_namespace = namespace_string.clone();
    let gc_supervision = supervision.clone();
    let gc_health = runtime_health.clone();
    let gc = tokio::spawn(async move {
        supervise_controller("owner_gc", gc_supervision, gc_health, move || {
            let collector = flotilla_store::OwnerGarbageCollector::new(gc_backend.clone(), gc_namespace.clone());
            async move { collector.run(controller_resync_interval).await }
        })
        .await;
    });

    let mut controllers = vec![
        gc,
        spawn_vessel_placement_projector(
            backend.clone(),
            namespace_string.clone(),
            state.local_host_ref.clone(),
            state.ssh_host_refs(),
            supervision.clone(),
            runtime_health.clone(),
        ),
        controller!(Repository, {
            let observed_backend = observed_backend.clone();
            let forge_default_branch_resolver = forge_default_branch_resolver.clone();
            move |backend: ResourceBackend, namespace_string: String| {
                let observed_backend = observed_backend.clone();
                let forge_default_branch_resolver = forge_default_branch_resolver.clone();
                let mut reconciler = RepositoryReconciler::new(backend, observed_backend.clone(), &namespace_string);
                if let Some(resolver) = forge_default_branch_resolver {
                    reconciler = reconciler.with_forge_default_branch_resolver(resolver);
                }
                (RepositoryReconciler::secondary_watches(observed_backend, &namespace_string), reconciler)
            }
        }),
        controller!(Environment, {
            let state = Arc::clone(&state);
            move |backend: ResourceBackend, namespace_string: String| {
                let local_host_ref = state.local_host_ref.clone();
                let additional_host_refs = state.ssh_host_refs();
                let lifecycle_reads = state.daemon.lifecycle_reads();
                let lifecycles = state.provisioning.as_ref().map(|engine| Arc::clone(&engine.environments));
                let state = Arc::clone(&state);
                (
                    vec![],
                    Engine::<_, EnvironmentBinding>::new(
                        EnvironmentReconciler::new(Arc::new(DockerControllerRuntime { state }), backend, &namespace_string)
                            .with_lifecycle_reads(lifecycle_reads)
                            .with_local_host_ref(CanonicalHostId::resolved(local_host_ref))
                            .with_additional_host_refs(additional_host_refs),
                        lifecycles,
                    ),
                )
            }
        }),
        controller!(Clone, {
            let state = Arc::clone(&state);
            move |backend: ResourceBackend, namespace_string: String| {
                (
                    vec![],
                    Engine::<_, CloneBinding>::new(
                        CloneReconciler::new(
                            Arc::new(RoutingCloneRuntime { state: Arc::clone(&state) }),
                            backend.using::<Repository>(&namespace_string),
                        )
                        .with_lifecycle_reads(state.daemon.lifecycle_reads()),
                        state.provisioning.as_ref().map(|engine| Arc::clone(&engine.clones)),
                    ),
                )
            }
        }),
        controller!(Checkout, {
            let state = Arc::clone(&state);
            move |backend: ResourceBackend, namespace_string: String| {
                let state = Arc::clone(&state);
                // Match background task admission to the shared daemon semaphore
                // initialized from this value by with_checkout_removal_concurrency.
                // A namespace-local task cap alone cannot pace all removals.
                let removal_concurrency = state.checkout_removal_concurrency;
                let lifecycle_reads = state.daemon.lifecycle_reads();
                let lifecycles = state.provisioning.as_ref().map(|engine| Arc::clone(&engine.checkouts));
                (
                    CheckoutReconciler::<RoutingCheckoutRuntime>::federated_secondary_watches(&backend, &namespace_string),
                    Engine::<_, CheckoutBinding>::new(
                        CheckoutReconciler::new(
                            Arc::new(RoutingCheckoutRuntime {
                                state,
                                change_requests: Some(backend.including_replicas::<ChangeRequest>(&namespace_string)),
                            }),
                            backend.clone(),
                            &namespace_string,
                        )
                        .with_lifecycle_reads(lifecycle_reads)
                        .with_background_removal_limit(removal_concurrency)
                        .with_federated_convoys(&backend, &namespace_string),
                        lifecycles,
                    ),
                )
            }
        }),
        controller!(TerminalSession, {
            let state = Arc::clone(&state);
            move |backend: ResourceBackend, namespace_string: String| {
                let local_host_ref = state.local_host_ref.clone();
                let additional_host_refs = state.ssh_host_refs();
                let state = Arc::clone(&state);
                let lifecycles = state.provisioning.as_ref().map(|engine| Arc::clone(&engine.terminal_sessions));
                let reads = state.daemon.lifecycle_reads();
                (
                    vec![],
                    Engine::<_, TerminalSessionBinding>::new(
                        TerminalSessionReconciler::new(Arc::new(TerminalControllerRuntime { state }), backend.clone(), &namespace_string)
                            .with_lifecycle_reads(reads)
                            .with_local_host_ref(CanonicalHostId::resolved(local_host_ref))
                            .with_additional_host_refs(additional_host_refs)
                            .with_federated_convoys(&backend, &namespace_string),
                        lifecycles,
                    ),
                )
            }
        }),
        controller!(Vessel, {
            let state = Arc::clone(&state);
            let config_dir = state.config.base_path().as_path().to_path_buf();
            let local_host_ref = state.local_host_ref.clone();
            let additional_host_refs = state.ssh_host_refs();
            move |backend: ResourceBackend, namespace_string: String| {
                let config_dir = config_dir.clone();
                let local_host_ref = local_host_ref.clone();
                let additional_host_refs = additional_host_refs.clone();
                let lifecycles = state.provisioning.as_ref().map(|engine| Arc::clone(&engine.vessels));
                (
                    VesselReconciler::secondary_watches(),
                    Engine::<_, VesselBinding>::new(
                        VesselReconciler::new_with_config_dir(backend.clone(), &namespace_string, config_dir)
                            .with_lifecycle_reads(state.daemon.lifecycle_reads())
                            .with_accounts(state.daemon.account_records())
                            .with_worktree_metadata(Arc::new(RoutingCheckoutRuntime { state: Arc::clone(&state), change_requests: None }))
                            .with_federated_dependencies(&backend, CanonicalHostId::resolved(local_host_ref))
                            .with_additional_host_refs(additional_host_refs),
                        lifecycles,
                    ),
                )
            }
        }),
        controller!(Convoy, {
            let daemon = Arc::clone(&state.daemon);
            move |backend: ResourceBackend, namespace_string: String| {
                let daemon = Arc::clone(&daemon);
                let mut secondaries = ConvoyReconciler::federated_secondary_watches(&backend, &namespace_string);
                secondaries.push(daemon.reconciler_wake_watch());
                secondaries.push(Box::new(flotilla_crew::convoy::EngineConvoyWake { engine: daemon.convoy_engine_path() }));
                (
                    secondaries,
                    ConvoyReconciler::new(backend.definitions::<WorkflowTemplate>(&namespace_string))
                        .with_engine(daemon.convoy_engine_path())
                        .with_messages(daemon.crew_message_path())
                        .with_lifecycle_reads(daemon.lifecycle_reads())
                        .with_hosts(backend.including_replicas::<Host>(&namespace_string))
                        .with_convoys(backend.including_replicas::<Convoy>(&namespace_string))
                        .with_vessels(backend.clone().using::<Vessel>(&namespace_string))
                        .with_federated_vessels(backend.including_replicas::<Vessel>(&namespace_string))
                        .with_terminal_sessions(backend.clone().using::<TerminalSession>(&namespace_string))
                        .with_checkouts(backend.clone().using::<Checkout>(&namespace_string))
                        .with_federated_checkouts(backend.including_replicas::<Checkout>(&namespace_string))
                        .with_forges(backend.definitions::<Forge>(&namespace_string))
                        .with_change_requests(
                            backend.including_replicas::<flotilla_resources::ChangeRequest>(&namespace_string),
                            daemon.change_request_stale_after(),
                        )
                        .with_artifacts(backend.including_replicas::<flotilla_resources::Artifact>(&namespace_string))
                        .with_landing_evidence_stale_after(LANDING_EVIDENCE_TTL)
                        .with_teardown_runtime(Arc::new(DaemonConvoyTeardownRuntime::new(daemon)))
                        .with_prepared_snapshot_gc(flotilla_store::PreparedSnapshotGarbageCollector::new(
                            backend.clone(),
                            &namespace_string,
                        )),
                )
            }
        }),
    ];
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
    // Every host collects retired ImageBuild records; hosts with a build
    // service also execute the builds placed on them.
    if let Some(credentials) = state.credential_store.clone() {
        let blobs: Arc<dyn crate::blob_store::BlobStore> = match state.blob_store.clone() {
            Some(blobs) => blobs,
            None => Arc::new(crate::blob_store::LocalBlobStore::new(state.config.state_dir().as_path())),
        };
        let materials: Arc<dyn flotilla_provisioning::reconcilers::ImageBuildMaterials> =
            Arc::new(crate::image_build::DaemonImageBuildMaterials {
                credentials,
                blobs,
                backend: backend.clone(),
                namespace: namespace_string.clone(),
                host: state.local_host_ref.clone(),
                secrets: image_builder_directory(state.config.state_dir().as_path()).join("secrets"),
            });
        let service = state.image_build_service.clone();
        let host = state.local_host_ref.clone();
        let probe_wrapper: Arc<str> = flotilla_agent::process::with_preludes("exec \"$@\"").into();
        let image_build_reads = state.daemon.lifecycle_reads();
        let image_build_lifecycles = state.provisioning.as_ref().map(|engine| Arc::clone(&engine.image_builds));
        controllers.push(controller!(flotilla_resources::ImageBuild, {
            move |backend: ResourceBackend, namespace_string: String| {
                (
                    vec![],
                    Engine::<_, ImageBuildBinding>::new(
                        flotilla_provisioning::reconcilers::ImageBuildReconciler::new(
                            service.clone(),
                            Arc::clone(&materials),
                            &backend,
                            &namespace_string,
                            &host,
                            &probe_wrapper,
                        )
                        .with_lifecycle_reads(Arc::clone(&image_build_reads)),
                        image_build_lifecycles.clone(),
                    ),
                )
            }
        }));
    } else {
        warn!("no credential store; image builds placed on this host will not run");
    }
    // Builders mirror replicated per-identity ImageBuild demand into their own
    // store, and withdraw copies whose demand was deleted.
    {
        let projection_backend = backend.clone();
        let projection_namespace = namespace_string.clone();
        let projection_host = state.local_host_ref.clone();
        let projection_supervision = supervision.clone();
        let projection_health = runtime_health.clone();
        controllers.push(tokio::spawn(async move {
            supervise_controller("image_build_placement", projection_supervision, projection_health, move || {
                let backend = projection_backend.clone();
                let namespace = projection_namespace.clone();
                let host = projection_host.clone();
                async move {
                    let mut interval = tokio::time::interval(Duration::from_secs(1));
                    loop {
                        interval.tick().await;
                        crate::image_build::project_builds(&backend, &namespace, &host).await?;
                    }
                    #[allow(unreachable_code)]
                    Ok(())
                }
            })
            .await;
        }));
    }
    // The old Message controller runs only while Message is in the old store
    // (#3317); behind `[primitives]` the engine path's delivery adapter takes over.
    if state.daemon.message_primitives().is_none() {
        controllers.push({
            let state = Arc::clone(&state);
            let health = runtime_health.clone();
            spawn_resource_controller::<flotilla_resources::Message, _, _>(
                backend.clone(),
                namespace_string.clone(),
                supervision.clone(),
                runtime_health.clone(),
                move |_backend, namespace| {
                    let controller = MessageController { state: Arc::clone(&state), namespace };
                    let heartbeat = Arc::new(AtomicU64::new(0));
                    let health = health.clone();
                    let metrics = state.controller_metrics.for_kind("Message");
                    async move {
                        let watchdog =
                            spawn_controller_loop_watchdog("Message", Arc::clone(&heartbeat), controller_resync_interval, health.clone());
                        let result = controller.run(controller_resync_interval, heartbeat, metrics).await;
                        watchdog.abort();
                        health.clear_controller_loop_stall("Message");
                        result
                    }
                },
            )
        });
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
            move |_backend, namespace| account_access::run_provision_controller(Arc::clone(&state), namespace, controller_resync_interval),
        )
    });
    // #3964: each access's delivery status, removed with its place or convoy.
    controllers.push(controller!(flotilla_resources::AccessDelivery, {
        let state = Arc::clone(&state);
        move |_backend: ResourceBackend, namespace: String| access_layer::status::status_controller(Arc::clone(&state), namespace)
    }));
    controllers
}

/// The decided store's kinds behind `[primitives]`: Message and AgentSession
/// (#3317), the provisioning lifecycle kinds and TerminalSession, the account
/// kinds (#3824 layer 1), and the engine's Vessel and Ensure (#3629), each
/// under its production code.
pub(crate) fn decided_registry() -> Result<flotilla_schema::KindRegistry, flotilla_schema::RegistryError> {
    let mut registry = flotilla_engine_store::registry()?;
    flotilla_provisioning::stored::register(&mut registry)?;
    flotilla_engine_store::register_crew(&mut registry)?;
    Ok(registry)
}

/// The machine diagram of each kind in [`decided_registry`], by kind name.
pub(crate) fn decided_diagrams() -> Result<std::collections::BTreeMap<String, String>, flotilla_schema::RegistryError> {
    let mut diagrams = flotilla_engine_store::machine_registry()?.diagrams;
    diagrams.extend(flotilla_provisioning::stored::diagrams());
    diagrams.extend(flotilla_engine_store::crew_diagrams());
    Ok(diagrams)
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
        for kind in ["Message", "AgentSession", "Environment", "ImageBuild", "Clone", "Checkout", "TerminalSession", "Vessel", "Ensure"] {
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

/// Opens the engine path behind `[primitives]` (#3317): the decided store
/// with the [`decided_registry`] kinds, homed at this daemon's node id, and the
/// Message path over it. Installed before peer tasks start, so every peer's
/// log is followed from the first connection.
async fn open_message_primitives(daemon: &Arc<InProcessDaemon>, config: &ConfigStore) -> Result<(), String> {
    let path = config.state_dir().as_path().join("decided.sqlite");
    let home = daemon.node_id().as_str().to_owned();
    let registry = decided_registry().map_err(|error| format!("decided store registry: {error:?}"))?;
    let service = Arc::new(
        flotilla_schema_sqlite::stream::Service::open(&path, &home, registry)
            .map_err(|error| format!("open decided store {}: {error}", path.display()))?,
    );
    daemon.install_decided_store(Arc::clone(&service))?;
    let primitives = retry_phase("restore_message_primitives", || async {
        let backend = daemon.resource_backend();
        let clock = daemon.clock_for_scenarios();
        let home = home.clone();
        let service = Arc::clone(&service);
        tokio::task::spawn_blocking(move || flotilla_core::primitives::MessagePrimitives::open(&home, service, backend, clock))
            .await
            .map_err(|error| error.to_string())?
    })
    .await?;
    daemon.install_message_primitives(Arc::new(primitives))
}

/// The archive shipper over the decided store (#3939), uploading to every configured fleet blob
/// store. None without `[primitives]`, whose store it archives, or without a store to ship to.
fn archive_shipper(
    daemon: &Arc<InProcessDaemon>,
    stores: &[flotilla_config::BlobStoreConfig],
    health: &RuntimeHealth,
) -> Result<Option<archive::ArchiveShipper>, String> {
    let Ok(decided) = daemon.decided_store() else { return Ok(None) };
    if stores.is_empty() {
        info!("no [[blob_stores]] configured: ended engine resources stay local");
        return Ok(None);
    }
    let targets = archive::targets(stores)?;
    let health = health.clone();
    Ok(Some(archive::ArchiveShipper::new(
        decided.store(),
        archive::archive_kinds(),
        targets,
        daemon.clock_for_scenarios(),
        Arc::new(move |condition| health.report_archive(condition)),
    )))
}

/// The provisioning kinds' engine homes over the decided store (#3629 layer 3), each restored
/// with the resources this home holds.
async fn open_provisioning_lifecycles(
    daemon: &Arc<InProcessDaemon>,
) -> Result<flotilla_provisioning::engine::ProvisioningLifecycles, String> {
    use flotilla_engine_store::{open_home, StoreLog};
    use flotilla_provisioning::engine::{reads::StoreReads, vessel::PlacementLifecycles, LifecycleHome, ProvisioningLifecycles};
    let store = daemon.decided_store()?.store();
    let home = StoreLog::new(Arc::clone(&store)).home().to_owned();
    daemon.install_lifecycle_reads(Arc::new(flotilla_provisioning::engine::reads::LifecycleReadPort::new(
        StoreReads::new(Arc::clone(&store)),
        home,
    )))?;
    daemon.install_vessel_seats(Arc::new(EngineVesselSeats {
        reads: StoreReads::new(Arc::clone(&store)),
        backend: daemon.resource_backend(),
    }))?;
    // #3824 layer 1: the account kinds on the engine, installed before their controllers start.
    let service = daemon.decided_store()?;
    let clock = daemon.clock_for_scenarios();
    let accounts = tokio::task::spawn_blocking(move || flotilla_provisioning::engine::account::EngineAccounts::open(service, clock))
        .await
        .map_err(|error| error.to_string())??;
    daemon.install_account_records(Arc::new(accounts))?;
    // #3824 layer 2c: ManifestRoot on the engine, installed before the manifest reconciler starts.
    let service = daemon.decided_store()?;
    let clock = daemon.clock_for_scenarios();
    let roots =
        tokio::task::spawn_blocking(move || flotilla_provisioning::engine::manifest_root::EngineManifestRoots::open(service, clock))
            .await
            .map_err(|error| error.to_string())??;
    daemon.install_manifest_roots(Arc::new(roots))?;
    // #3824 layer 2b-ii: the definition kinds on the engine, installed on the daemon's backend so
    // every definition read and write reaches them.
    let service = daemon.decided_store()?;
    let clock = daemon.clock_for_scenarios();
    let definitions =
        tokio::task::spawn_blocking(move || flotilla_provisioning::engine::definitions::EngineDefinitions::open(service, clock))
            .await
            .map_err(|error| error.to_string())??;
    daemon.resource_backend().install_engine_records(Arc::new(definitions));
    // #3824 layer 3a: Repository likewise.
    let service = daemon.decided_store()?;
    let clock = daemon.clock_for_scenarios();
    let repositories =
        tokio::task::spawn_blocking(move || flotilla_provisioning::engine::repositories::EngineRepositories::open(service, clock))
            .await
            .map_err(|error| error.to_string())??;
    daemon.resource_backend().install_engine_records(Arc::new(repositories));
    // #3824 layer 3b: Project, with project operations' record and the dispatch queue's judgement.
    let service = daemon.decided_store()?;
    let clock = daemon.clock_for_scenarios();
    let projects = tokio::task::spawn_blocking(move || flotilla_provisioning::engine::projects::EngineProjects::open(service, clock))
        .await
        .map_err(|error| error.to_string())??;
    daemon.resource_backend().install_engine_records(Arc::new(projects));
    let backend = daemon.resource_backend();
    tokio::task::spawn_blocking(move || {
        let home = StoreLog::new(Arc::clone(&store)).home().to_owned();
        let log = || StoreLog::new(Arc::clone(&store));
        let terminal_sessions = Arc::new(LifecycleHome::new(&home, open_home(log())?));
        backend.install_terminal_launches(Arc::new(flotilla_provisioning::engine::terminal_session::EngineTerminalLaunches {
            lifecycles: terminal_sessions.clone(),
        }));
        Ok(ProvisioningLifecycles {
            environments: Arc::new(LifecycleHome::new(&home, open_home(log())?)),
            image_builds: Arc::new(LifecycleHome::new(&home, open_home(log())?)),
            clones: Arc::new(LifecycleHome::new(&home, open_home(log())?)),
            checkouts: Arc::new(LifecycleHome::new(&home, open_home(log())?)),
            terminal_sessions,
            vessels: Arc::new(PlacementLifecycles::new(&home, open_home(log())?)?),
        })
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Which seats a vessel serves (#3629 layer 8): its sessions by their vessel label, with each role
/// the engine holds a launch for (`terminal_session.by_holder`, the vessel as holder) served by
/// that launch's old session record instead. During the switch a vessel can hold launches the
/// engine decided beside sessions only the old store knows (from before the switch, or homed
/// elsewhere), so the labels are the base and the engine overlays them; an engine launch whose
/// record is gone leaves the labels' answer.
struct EngineVesselSeats {
    reads: flotilla_provisioning::engine::reads::StoreReads,
    backend: ResourceBackend,
}

#[async_trait::async_trait]
impl flotilla_resources::vessel_seats::VesselSeats for EngineVesselSeats {
    async fn seats(
        &self,
        namespace: &str,
        vessel_ref: &str,
    ) -> Result<std::collections::BTreeMap<String, flotilla_resources::ReadResourceObject<flotilla_resources::TerminalSession>>, String>
    {
        use flotilla_provisioning::{
            engine::{launched, reads::ProvisioningReads},
            stored::Holder,
        };
        let sessions = self.backend.including_replicas::<flotilla_resources::TerminalSession>(namespace);
        let labelled = sessions
            .list_matching_labels(&std::collections::BTreeMap::from([(
                flotilla_resources::VESSEL_REF_LABEL.to_string(),
                vessel_ref.to_owned(),
            )]))
            .await
            .map_err(|error| error.to_string())?
            .items;
        let views = self.reads.held_sessions(&Holder::vessel(vessel_ref)).map_err(|error| error.to_string())?;
        let mut names: Vec<String> = views.iter().map(|view| launched(&view.name).to_owned()).collect();
        names.sort();
        names.dedup();
        let mut seats: std::collections::BTreeMap<_, _> =
            labelled.into_iter().map(|session| (session.object.spec.role.clone(), session)).collect();
        for name in names {
            match sessions.get(&name).await {
                Ok(session) => {
                    seats.insert(session.object.spec.role.clone(), session);
                }
                Err(ResourceError::NotFound { .. }) => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(seats)
    }
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

/// Optional provider-only Drydock configuration. Supervision remains at the
/// composition root; the adapter sees only ProviderApi and host readiness.
#[cfg(unix)]
async fn compose_drydock(
    config: &ConfigStore,
    registry: &mut ProviderRegistry,
    env: &dyn flotilla_provider_runtime::EnvVars,
) -> Result<(), String> {
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
    if configuration.provider.is_empty() {
        return Ok(());
    }
    let mut providers = Vec::new();
    let mut credentials = std::collections::BTreeMap::new();
    let mut modules = std::collections::BTreeMap::new();
    for (name, provider) in &configuration.provider {
        modules.insert(
            provider.module.clone(),
            drydock_protocol::ModuleConfiguration { name: provider.module.clone(), entrypoint: provider.module.clone() },
        );
        credentials.insert(name.clone(), provider.credential.reference(env)?);
        providers.push(drydock_protocol::ProviderConfiguration {
            name: name.clone(),
            module: provider.module.clone(),
            settings: provider.settings.clone(),
            credential: drydock_protocol::CredentialMode::Direct { credential_reference: name.clone() },
        });
    }
    let binary = drydock::local::companion("drydock-host", "DRYDOCK_HOST_BIN").unwrap_or_else(|_| PathBuf::from("drydock-host"));
    let supervisor = drydock::Supervisor::new(
        config.state_dir().as_path().join("drydock-hosts"),
        Arc::new(drydock::credentials::BuiltinCredentials::default()),
    );
    let host = Arc::new(
        supervisor
            .start(drydock::HostSpec {
                name: "rust".into(),
                connection: drydock::HostConnection::Managed {
                    command: vec![binary.to_string_lossy().into_owned()],
                    runtime: None,
                    proxy_environment: Default::default(),
                },
                configuration: drydock_protocol::HostConfiguration { modules: modules.into_values().collect(), providers },
                credentials,
            })
            .await
            .map_err(|error| error.to_string())?,
    );
    for (name, provider) in configuration.provider {
        if registry.environment_providers.contains_key(&name) {
            return Err(format!("duplicate environment provider instance {name}"));
        }
        // Drydock's configuration owns how its vessels' agents are reached.
        let reach = drydock::agent::AgentReach::new(provider.agent_link);
        let adapter: Arc<dyn EnvironmentProvider> = Arc::new(
            DrydockEnvironmentProvider::new(
                Arc::new(host.client.clone()),
                name.clone(),
                provider.module,
                config.state_dir().as_path().join("drydock-keys"),
            )
            .with_agent_reach(reach)
            .with_host(host.clone())
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
