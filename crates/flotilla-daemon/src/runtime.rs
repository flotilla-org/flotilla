//! Daemon composition, startup restoration, and resource-controller wiring.

mod credentials;
mod discovery;
mod docker;
mod environments;
mod health;
mod message;
mod ports;
mod seed;
mod state;
mod tasks;
mod terminal;
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{atomic::AtomicU64, Arc},
    time::Duration,
};

use chrono::Utc;
use flotilla_controllers::reconcilers::{
    convoy_ensure::EnsureReconciler, CheckoutReconciler, CloneReconciler, EnvironmentReconciler, ForgeDefaultBranchResolver,
    RepositoryReconciler, TerminalSessionReconciler, VesselReconciler,
};
use flotilla_core::{
    checkout_integration::LANDING_EVIDENCE_TTL, config::ConfigStore, in_process::InProcessDaemon, providers::registry::ProviderRegistry,
};
use flotilla_credentials::{AgentMaterialRegistry, CredentialStore};
use flotilla_paths::path_context::DaemonHostPath;
use flotilla_paths::path_context::ExecutionEnvironmentPath;
use flotilla_protocol::CanonicalHostId;
use flotilla_resources::{
    host_direct_environment_name, ChangeRequest, Checkout, Clone, Convoy, Environment, Forge, Host, ManifestRoot, Repository, Resource,
    ResourceError, SystemClock, TerminalSession, Vessel, WorkflowTemplate,
};
use flotilla_store::{controller::ControllerLoop, ConvoyReconciler, ResourceBackend};
use tokio::{sync::watch, task::JoinHandle};
use tokio_util::task::AbortOnDropHandle;
use tracing::{debug, error, info, warn};

use self::credentials::{
    reconcile_work_credentials, spawn_codex_central_refresh_task, spawn_codex_credential_redelivery_task, spawn_credential_refresh_task,
    RuntimeSessionCapabilities, RuntimeWorkCredentialReconciler,
};
use self::discovery::{
    apply_agentless_ssh_observation, build_local_profile, discover_agentless_ssh_profiles, probe_local_provider_registry,
    register_agentless_ssh_resources,
};
use self::docker::DockerControllerRuntime;
use self::environments::reconcile_provisioned_environments;
use self::health::{
    apply_host_heartbeat_with_credentials, spawn_controller_loop_watchdog, spawn_projection_parity_task, DaemonHealthIdentity,
    RuntimeHealth,
};
use self::message::{MessageController, MessageDependencyWatch};
use self::ports::{
    DaemonConvoyTeardownRuntime, GhForgeDefaultBranchResolver, RoutingCheckoutRuntime, RoutingCloneRuntime, RuntimeOperatorReconciler,
};
use self::seed::register_startup_resources;
use self::state::ControllerRuntimeState;
use self::tasks::{
    run_artifact_gc, run_checkout_archive_gc, spawn_adopted_checkout_reconciliation_task, spawn_aggregator_task,
    spawn_blob_sync_status_task, spawn_convoy_ensure_reconciler_task, spawn_demand_expiry_task, spawn_dispatch_reconciler_task,
    spawn_environment_orphan_sweep_task, spawn_event_expiry_task, spawn_heartbeat_task_with_credentials,
    spawn_host_description_projection_task, spawn_liveness_watchdog_task, spawn_local_fulfilment_probe_task,
    spawn_manifest_reconciler_task, spawn_periodic_task, spawn_provisioned_environment_reconciliation_task, spawn_resource_controller,
    spawn_sleep_inhibitor_task, spawn_ssh_fulfilment_probe_task, spawn_startup_restoration, spawn_vessel_placement_projector,
    supervise_controller, CheckoutArchiveRoot, CheckoutArchiveSweep, PeriodicTaskStart, LIVENESS_WATCHDOG_INTERVAL,
    MANIFEST_RECONCILE_INTERVAL,
};
use self::terminal::TerminalControllerRuntime;
use crate::{blob_store::TieredBlobStore, resource_manifest::manifest_root_name, startup::phase, supervisor::ControllerSupervision};
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
        phase("initialize_runtime", Self::start_with_options(daemon, config, daemon_socket_path, RuntimeOptions::default())).await
    }

    pub async fn start_with_options(
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
        let manifests = daemon_config.manifests;
        let relay = daemon_config.relay;
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

        let local_registry = phase("probe_local_provider_registry", probe_local_provider_registry(&daemon, &config)).await?;
        let profile = build_local_profile(&daemon, &local_registry)?;
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
        let ssh_profiles = phase("discover_agentless_ssh_profiles", discover_agentless_ssh_profiles(&daemon, &config)).await;
        daemon.set_admission_free_space_path(PathBuf::from(&profile.repo_default_dir));
        let credential_store = Arc::new(CredentialStore::new(
            daemon.resource_backend(),
            &options.namespace,
            Arc::clone(&daemon.discovery_runtime().env),
            daemon.local_environment_bag().ok_or_else(|| "local environment bag unavailable".to_string())?,
            daemon.local_command_runner().ok_or_else(|| "local command runner unavailable".to_string())?,
            config.state_dir().as_path().to_path_buf(),
        ));
        // Cleanup must precede admitting commands that can mint new staging files.
        if let Err(error) = phase("cleanup_stale_github_app_token_files", credential_store.cleanup_stale_github_app_token_files()).await {
            warn!(%error, "failed to clean up stale GitHub App token staging files");
        }
        let agent_material = Arc::new(AgentMaterialRegistry::new(Arc::clone(&daemon.discovery_runtime().env)));
        let health = DaemonHealthIdentity {
            generation: phase("read_observed_generation", daemon.observed_resource_backend().using::<Checkout>(&options.namespace).list())
                .await
                .map_err(|error| error.to_string())?
                .generation,
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: Utc::now(),
        };
        phase(
            "set_local_placement_capabilities",
            daemon.set_local_placement_capabilities(&profile.available_agent_adapters, &profile.available_pools),
        )
        .await;
        let runtime_health = RuntimeHealth::default().with_restart_history_dir(config.state_dir().as_path().to_path_buf());
        phase(
            "quarantine_undecodable_stored_objects",
            flotilla_store::quarantine_undecodable_stored_objects(&daemon.resource_backend(), &options.namespace),
        )
        .await
        .map_err(|error| format!("scan stored resources for decode quarantine: {error}"))?;
        phase("register_startup_resources", register_startup_resources(&daemon, &options.namespace, &profile)).await?;
        let registered_image_caches = local_registry
            .environment_providers
            .iter()
            .filter(|(_, provider)| provider.local_image_cache().is_some())
            .filter_map(|(_, provider)| local_registry.environment_providers.instance_name(provider).map(String::from))
            .collect::<BTreeSet<_>>();
        crate::image_distribution::prune_unregistered_caches(
            &daemon.resource_backend(),
            &options.namespace,
            &profile.host_id,
            &registered_image_caches,
        )
        .await?;
        let mut registered_ssh_profiles = Vec::new();
        for ssh in ssh_profiles {
            if let Err(error) = phase(
                "register_agentless_ssh_resources",
                register_agentless_ssh_resources(&daemon.resource_backend(), &options.namespace, &profile.host_id, &ssh),
            )
            .await
            {
                warn!(host = %ssh.provisioning.host_id, %error, "failed to register agentless SSH host; continuing startup");
                continue;
            }
            if let Err(error) = phase(
                "apply_agentless_ssh_observation",
                apply_agentless_ssh_observation(&daemon, &options.namespace, &ssh, Some(&credential_store)),
            )
            .await
            {
                warn!(host = %ssh.provisioning.host_id, %error, "failed to observe agentless SSH host; continuing startup");
            }
            registered_ssh_profiles.push(ssh);
        }
        let ssh_profiles = registered_ssh_profiles;
        phase(
            "publish_local_host_heartbeat",
            apply_host_heartbeat_with_credentials(&daemon, &options.namespace, &profile, Some(&credential_store), &health, &runtime_health),
        )
        .await?;

        let mut tasks = vec![
            spawn_local_fulfilment_probe_task(
                Arc::clone(&daemon),
                options.namespace.clone(),
                profile.clone(),
                config.state_dir().as_path().join("probe-cwd"),
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
            spawn_blob_sync_status_task(
                Arc::clone(&blob_store),
                daemon.resource_backend(),
                options.namespace.clone(),
                profile.host_id.clone(),
            ),
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
        if let Some(relay) = relay {
            tasks.push(crate::event_relay::spawn(Arc::clone(&daemon), relay, config.state_dir().as_path().to_path_buf())?);
        }
        for ssh in &ssh_profiles {
            tasks.push(spawn_ssh_fulfilment_probe_task(Arc::clone(&daemon), options.namespace.clone(), ssh.clone()));
            let daemon = Arc::clone(&daemon);
            let namespace = options.namespace.clone();
            let ssh = ssh.clone();
            let credential_store = Arc::clone(&credential_store);
            tasks.push(spawn_periodic_task(options.heartbeat_interval, PeriodicTaskStart::AfterInterval, move || {
                let daemon = Arc::clone(&daemon);
                let namespace = namespace.clone();
                let ssh = ssh.clone();
                let credential_store = Arc::clone(&credential_store);
                async move {
                    if let Err(error) = apply_agentless_ssh_observation(&daemon, &namespace, &ssh, Some(&credential_store)).await {
                        warn!(host = %ssh.provisioning.host_id, %error, "failed to publish SSH host observation");
                    }
                }
            }));
        }
        let desired_manifest_root = manifests
            .as_ref()
            .filter(|declared| manifest_reconciler_enabled(&declared.reconciler_root, &profile.host_id))
            .map(|declared| manifest_root_name(&declared.reconciler_root, &declared.dir, &declared.source));
        let roots = daemon.resource_backend().using::<ManifestRoot>(&options.namespace);
        for root in phase("list_manifest_roots", roots.list()).await.map_err(|error| error.to_string())?.items {
            if root.metadata.name.starts_with("manifest-")
                && root.spec.host == profile.host_id
                && desired_manifest_root.as_deref() != Some(root.metadata.name.as_str())
            {
                phase("delete_stale_manifest_root", roots.delete(&root.metadata.name)).await.map_err(|error| error.to_string())?;
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

        let mut controller_state = None;
        daemon
            .install_convoy_ensure_reconciler(Arc::new(
                EnsureReconciler::builder().resource_backend(daemon.resource_backend()).clock(Arc::new(SystemClock)).build(),
            ))
            .await;
        if options.start_controllers {
            let image_build_runner = {
                use flotilla_core::{
                    providers::vcs::git_worktree::GitWorktreeStrategy,
                    vcs::{FlotillaVcs, GitCheckoutStrategy},
                };
                let runner = daemon.local_command_runner().ok_or("image build command runner unavailable")?;
                let directory = config.state_dir().as_path().join("image-builds");
                let vcs = Arc::new(FlotillaVcs::new(
                    ExecutionEnvironmentPath::new(&directory),
                    Arc::clone(&runner),
                    GitCheckoutStrategy::Worktree(Box::new(GitWorktreeStrategy::new(".".into(), Arc::clone(&runner)))),
                ));
                let distributor = if let Some((_, provider)) = local_registry
                    .environment_providers
                    .for_kind(flotilla_core::providers::environment::EnvironmentKind::Docker)
                    .filter(|(_, provider)| provider.local_image_cache().is_some())
                {
                    Some(Arc::new(
                        crate::image_distribution::ImageDistributor::builder()
                            .io(Arc::new(
                                crate::image_distribution::ProviderImageIo::builder()
                                    .provider(Arc::clone(provider))
                                    .maybe_publisher(provider.image_builder())
                                    .credentials(Arc::clone(&credential_store))
                                    .host(profile.host_id.clone())
                                    .build(),
                            ))
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
                    ))
                } else {
                    // Host-direct has no local image cache. An ambiguous provider
                    // registry also cannot attest which instance owns an observation.
                    None
                };
                Arc::new(crate::image_build::LocalImageBuildRunner {
                    cache_name: distributor.as_ref().map(|d| d.provider_instance.clone()),
                    builder: distributor.as_ref().and_then(|d| d.io.publisher.clone()),
                    distributor,
                    vcs,
                    directory,
                    backend: daemon.resource_backend(),
                    namespace: options.namespace.clone(),
                    blobs: Arc::clone(&blob_store),
                })
            };
            daemon.set_image_build_input_resolver(image_build_runner.clone()).await;
            let state = Arc::new(
                ControllerRuntimeState::new(
                    Arc::clone(&daemon),
                    Arc::clone(&config),
                    Arc::clone(&local_registry),
                    daemon_socket_path.map(DaemonHostPath::new),
                    profile.host_id.clone(),
                    profile.host_direct_environment_name(),
                )
                .with_checkout_removal_concurrency(daemon_config.checkout_removal_concurrency)
                .with_namespace(options.namespace.clone())
                .with_agentless_ssh(ssh_profiles.clone())
                .with_credential_store(Arc::clone(&credential_store))
                .with_agent_material(agent_material)
                .with_blob_store(Arc::clone(&blob_store))
                .with_image_build_runner(image_build_runner),
            );
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

        Ok(Self { tasks, blob_store, stop_expected: false })
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
            local_registry.environment_providers.for_kind(flotilla_core::providers::environment::EnvironmentKind::Docker)
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
    let observed_backend = state.daemon.observed_resource_backend();
    let forge_default_branch_resolver = state
        .daemon
        .local_command_runner()
        .map(|runner| Arc::new(GhForgeDefaultBranchResolver { runner }) as Arc<dyn ForgeDefaultBranchResolver>);
    let namespace_string = namespace.to_string();

    macro_rules! controller {
        ($primary:ty, $make_parts:expr) => {{
            let make_parts = $make_parts;
            let loop_health = runtime_health.clone();
            spawn_resource_controller::<$primary, _, _>(
                backend.clone(),
                namespace_string.clone(),
                supervision.clone(),
                runtime_health.clone(),
                move |backend, namespace| {
                    let (secondaries, reconciler) = make_parts(backend.clone(), namespace.clone());
                    let controller = ControllerLoop {
                        primary: backend.clone().using::<$primary>(&namespace),
                        secondaries,
                        reconciler,
                        resync_interval: controller_resync_interval,
                        backend,
                    };
                    let heartbeat = Arc::new(AtomicU64::new(0));
                    let health = loop_health.clone();
                    async move {
                        let watchdog = spawn_controller_loop_watchdog(
                            <$primary>::API_PATHS.kind,
                            Arc::clone(&heartbeat),
                            controller_resync_interval,
                            health.clone(),
                        );
                        let result = controller.run_with_heartbeat(heartbeat).await;
                        watchdog.abort();
                        health.clear_controller_loop_stall(<$primary>::API_PATHS.kind);
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
            state.agentless_host_refs(),
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
                let additional_host_refs = state.agentless_host_refs();
                let image_inputs =
                    state.image_build_runner.clone().map(|runner| runner as Arc<dyn flotilla_core::image_build::ImageBuildInputResolver>);
                let state = Arc::clone(&state);
                (
                    vec![],
                    EnvironmentReconciler::new(Arc::new(DockerControllerRuntime { state }), backend, &namespace_string)
                        .with_image_build_inputs(image_inputs)
                        .with_local_host_ref(CanonicalHostId::resolved(local_host_ref))
                        .with_additional_host_refs(additional_host_refs),
                )
            }
        }),
        controller!(Clone, {
            let state = Arc::clone(&state);
            move |backend: ResourceBackend, namespace_string: String| {
                (
                    vec![],
                    CloneReconciler::new(
                        Arc::new(RoutingCloneRuntime { state: Arc::clone(&state) }),
                        backend.using::<Repository>(&namespace_string),
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
                (
                    CheckoutReconciler::<RoutingCheckoutRuntime>::federated_secondary_watches(&backend, &namespace_string),
                    CheckoutReconciler::new(
                        Arc::new(RoutingCheckoutRuntime {
                            state,
                            change_requests: Some(backend.including_replicas::<ChangeRequest>(&namespace_string)),
                        }),
                        backend.clone(),
                        &namespace_string,
                    )
                    .with_background_removal_limit(removal_concurrency)
                    .with_federated_convoys(&backend, &namespace_string),
                )
            }
        }),
        controller!(flotilla_resources::Message, {
            let state = Arc::clone(&state);
            move |_backend: ResourceBackend, namespace: String| {
                (
                    vec![
                        MessageDependencyWatch::<flotilla_resources::Message>::boxed(),
                        MessageDependencyWatch::<TerminalSession>::boxed(),
                        MessageDependencyWatch::<Convoy>::boxed(),
                        MessageDependencyWatch::<flotilla_resources::ConvoyEnsure>::boxed(),
                        MessageDependencyWatch::<ChangeRequest>::boxed(),
                        MessageDependencyWatch::<flotilla_resources::Issue>::boxed(),
                        MessageDependencyWatch::<flotilla_resources::Artifact>::boxed(),
                        MessageDependencyWatch::<flotilla_resources::Usage>::boxed(),
                        MessageDependencyWatch::<Vessel>::boxed(),
                    ],
                    MessageController { state: Arc::clone(&state), namespace },
                )
            }
        }),
        controller!(TerminalSession, {
            let state = Arc::clone(&state);
            move |backend: ResourceBackend, namespace_string: String| {
                let local_host_ref = state.local_host_ref.clone();
                let additional_host_refs = state.agentless_host_refs();
                let state = Arc::clone(&state);
                (
                    vec![],
                    TerminalSessionReconciler::new(Arc::new(TerminalControllerRuntime { state }), backend.clone(), &namespace_string)
                        .with_local_host_ref(CanonicalHostId::resolved(local_host_ref))
                        .with_additional_host_refs(additional_host_refs)
                        .with_federated_convoys(&backend, &namespace_string),
                )
            }
        }),
        controller!(Vessel, {
            let state = Arc::clone(&state);
            let config_dir = state.config.base_path().as_path().to_path_buf();
            let local_host_ref = state.local_host_ref.clone();
            let additional_host_refs = state.agentless_host_refs();
            move |backend: ResourceBackend, namespace_string: String| {
                let config_dir = config_dir.clone();
                let local_host_ref = local_host_ref.clone();
                let additional_host_refs = additional_host_refs.clone();
                (
                    VesselReconciler::secondary_watches(),
                    VesselReconciler::new_with_config_dir(backend.clone(), &namespace_string, config_dir)
                        .with_worktree_metadata(Arc::new(RoutingCheckoutRuntime { state: Arc::clone(&state), change_requests: None }))
                        .with_federated_dependencies(&backend, CanonicalHostId::resolved(local_host_ref))
                        .with_additional_host_refs(additional_host_refs),
                )
            }
        }),
        controller!(Convoy, {
            let daemon = Arc::clone(&state.daemon);
            move |backend: ResourceBackend, namespace_string: String| {
                let daemon = Arc::clone(&daemon);
                let mut secondaries = ConvoyReconciler::federated_secondary_watches(&backend, &namespace_string);
                secondaries.push(daemon.reconciler_wake_watch());
                (
                    secondaries,
                    ConvoyReconciler::new(backend.definitions::<WorkflowTemplate>(&namespace_string))
                        .with_hosts(backend.including_replicas::<Host>(&namespace_string))
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
    if let Some(distributor) = &state.image_distributor {
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
    if let Some(runner) = &state.image_build_runner {
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

        let runner = Arc::clone(runner);
        let host = state.local_host_ref.clone();
        controllers.push(controller!(flotilla_resources::ImageBuild, move |backend: ResourceBackend, namespace_string: String| {
            (vec![], flotilla_controllers::reconcilers::ImageBuildReconciler::new(Arc::clone(&runner), backend, &namespace_string, &host))
        }));
    }
    controllers
}

#[cfg(test)]
mod test_git_repo;
#[cfg(test)]
mod tests;
