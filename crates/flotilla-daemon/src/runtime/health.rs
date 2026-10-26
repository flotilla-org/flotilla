//! Runtime health, heartbeat conditions, and projection parity.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::Duration,
};

use chrono::Utc;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_projections::aggregator_projection::AggregatorProjectionState;
use flotilla_projections::IssuePollingHealth;

use flotilla_credentials::CredentialStore;
use flotilla_protocol::{BlobSyncStatus, HostSummary, NodeId, Rows};
use flotilla_resources::Resource;
use flotilla_resources::{
    ConditionValue, Convoy, CredentialExpiry, FoldedHostObservations, Forge, FulfilmentFacts, Host, HostCondition, HostStatus,
    HostStatusPatch, ManifestRoot, ReplicationClass, ResourceError, SystemClock, Vessel, AGENT_ADAPTERS_CAPABILITY,
    CREDENTIAL_EXPIRY_CAPABILITY, HELD_CREDENTIALS_CAPABILITY, LOCAL_ROWS_MISSING_REASON, PROJECTION_PARITY_CONDITION,
};
use flotilla_store::{home_bound_authorship_collisions, ResourceBackend, REGISTERED_RESOURCE_KINDS};
use serde_json::json;
use tokio::task::JoinHandle;
use tracing::{error, warn};

use super::discovery::LocalProvisioningProfile;
use super::seed::{ensure_host_exists, migrate_live_placement_policies};
use super::tasks::{spawn_periodic_task, PeriodicTaskStart, AGGREGATOR_CONTROLLER, CONVOY_ENSURE_CONDITION_TYPE};
use crate::{
    blob_store::TieredBlobStore, resource_limits::file_descriptor_pressure_condition, resource_manifest::manifest_root_name,
    supervisor::RestartBudgetExhausted,
};

/// An engine-held build's failure and cause come from the same judgement.
pub(super) fn image_build_failure_condition(
    build: &flotilla_resources::ResourceObject<flotilla_resources::ImageBuild>,
    lifecycles: &dyn flotilla_resources::lifecycle_reads::LifecycleReads,
) -> Option<HostCondition> {
    use flotilla_protocol::LifecycleReading;
    use flotilla_resources::lifecycle_reads::ProvisioningKind;
    let (reason, message) = match lifecycles.try_reading(ProvisioningKind::ImageBuild, &build.metadata.name, None) {
        // A failed read must remain visible without suppressing the other heartbeat observations.
        Err(error) => ("LifecycleReadFailed", error),
        Ok(Some(
            LifecycleReading::Exhausted { cause } | LifecycleReading::GaveUp { cause } | LifecycleReading::Failed { reason: cause },
        )) => ("ImageBuildFailed", cause),
        Ok(Some(_)) => return None,
        Ok(None) => {
            let status = build.status.as_ref().filter(|status| status.phase == flotilla_resources::ImageBuildPhase::Failed)?;
            ("ImageBuildFailed", status.failure.as_ref().map(|failure| failure.reason.clone()).unwrap_or_default())
        }
    };
    Some(
        HostCondition::builder()
            .condition_type(format!("ImageBuild/{}", build.metadata.name))
            .value(ConditionValue::False)
            .reason(reason)
            .message(message)
            .observed_at(Utc::now())
            .blocks_readiness(false)
            // A failed image build or read does not disable forge reads.
            .blocks_forge_observation(false)
            .build(),
    )
}

#[derive(Debug, Clone)]
pub(super) struct DaemonHealthIdentity {
    pub(super) generation: Option<String>,
    pub(super) version: String,
    pub(super) started_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct RuntimeHealth {
    pub(super) failures: Arc<StdMutex<BTreeMap<String, HostCondition>>>,
    pub(super) restart_history_dir: Option<Arc<PathBuf>>,
    pub(super) issue_polling: IssuePollingHealth,
    pub(super) host_observations: HostObservations,
}

/// Latest periodic observations of the local Host, folded into the heartbeat's
/// status write so each observer does not commit the Host object on its own
/// cadence (#3154). Host status is "latest value only" (WORKLOAD.md).
#[derive(Clone, Default)]
pub(super) struct HostObservations {
    blob_store: Option<Arc<TieredBlobStore>>,
    provider_advertisements: Arc<tokio::sync::RwLock<Option<ProviderObservation>>>,
    fulfilment_facts: Arc<tokio::sync::RwLock<Option<BTreeMap<String, FulfilmentFacts>>>>,
}

#[derive(Clone)]
struct ProviderObservation {
    at: tokio::time::Instant,
    providers: Vec<flotilla_protocol::provider::ProviderAdvertisement>,
}

impl std::fmt::Debug for HostObservations {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("HostObservations").field("blob_store", &self.blob_store.is_some()).finish_non_exhaustive()
    }
}

impl HostObservations {
    pub(super) fn with_blob_store(mut self, blob_store: Arc<TieredBlobStore>) -> Self {
        self.blob_store = Some(blob_store);
        self
    }

    async fn blob_sync(&self) -> Option<BlobSyncStatus> {
        match &self.blob_store {
            Some(store) => Some(store.status().await),
            None => None,
        }
    }

    pub(super) async fn record_provider_advertisements(&self, providers: Vec<flotilla_protocol::provider::ProviderAdvertisement>) {
        *self.provider_advertisements.write().await = Some(ProviderObservation { at: tokio::time::Instant::now(), providers });
    }

    async fn provider_advertisements(&self) -> Option<Vec<flotilla_protocol::provider::ProviderAdvertisement>> {
        let observed = self.provider_advertisements.read().await;
        let observation = observed.as_ref()?;
        let mut providers = observation.providers.clone();
        if observation.at.elapsed() >= Duration::from_secs(flotilla_resources::HEARTBEAT_READY_TTL_SECS as u64) {
            for provider in &mut providers {
                provider.readiness =
                    flotilla_protocol::provider::ProviderReadiness::Unknown { reason: "provider readiness observation expired".into() };
            }
        }
        Some(providers)
    }

    /// The facts most recently observed by this process, if any pass has completed.
    pub(super) async fn fulfilment_facts(&self) -> Option<BTreeMap<String, FulfilmentFacts>> {
        self.fulfilment_facts.read().await.clone()
    }

    pub(super) async fn record_fulfilment_facts(&self, facts: BTreeMap<String, FulfilmentFacts>) {
        *self.fulfilment_facts.write().await = Some(facts);
    }
}

impl RuntimeHealth {
    pub(super) fn with_blob_store(mut self, blob_store: Arc<TieredBlobStore>) -> Self {
        self.host_observations = self.host_observations.with_blob_store(blob_store);
        self
    }

    pub(super) fn report_capability_regression(&self, condition: HostCondition) {
        self.failures.lock().expect("runtime health lock poisoned").insert(condition.condition_type.clone(), condition);
    }

    pub(super) fn with_restart_history_dir(mut self, path: PathBuf) -> Self {
        self.restart_history_dir = Some(Arc::new(path));
        self
    }

    pub(super) fn report_restart_budget_exhausted(&self, exhausted: RestartBudgetExhausted) {
        let condition_type = format!("Controller/{}", exhausted.controller);
        let condition = HostCondition::builder()
            .condition_type(condition_type.clone())
            .value(ConditionValue::False)
            .reason("RestartBudgetExhausted")
            .message(format!(
                "{} controller exhausted its budget after {} consecutive failures; retrying after long backoff: {}",
                exhausted.controller, exhausted.attempts, exhausted.error
            ))
            .observed_at(Utc::now())
            // Only the aggregator schedules forge refresh; actuator-controller failure does not stop reads.
            .blocks_forge_observation(exhausted.controller == AGGREGATOR_CONTROLLER)
            .build();
        self.failures.lock().expect("runtime health lock poisoned").insert(condition_type, condition);
    }

    pub(super) fn report_controller_loop_stall(&self, name: &str, elapsed: Duration) {
        let condition_type = format!("ControllerLoop/{name}");
        let condition = HostCondition::builder()
            .condition_type(condition_type.clone())
            .value(ConditionValue::False)
            .reason("HeartbeatStale")
            .message(format!("{name} controller loop made no progress for {elapsed:?}"))
            .observed_at(Utc::now())
            // A stalled actuator reconcile does not stop the independent forge reader.
            .blocks_forge_observation(false)
            .build();
        self.failures.lock().expect("runtime health lock poisoned").insert(condition_type, condition);
    }

    pub(super) fn clear_controller_loop_stall(&self, name: &str) {
        self.failures.lock().expect("runtime health lock poisoned").remove(&format!("ControllerLoop/{name}"));
    }

    pub(super) fn report_projection_parity(&self, condition: Option<HostCondition>) {
        let mut failures = self.failures.lock().expect("runtime health lock poisoned");
        match condition {
            Some(condition) => {
                failures.insert(PROJECTION_PARITY_CONDITION.to_string(), condition);
            }
            None => {
                failures.remove(PROJECTION_PARITY_CONDITION);
            }
        }
    }

    pub(super) fn report_convoy_ensure_timeout(&self, timeout: Duration) {
        let condition = HostCondition::builder()
            .condition_type(CONVOY_ENSURE_CONDITION_TYPE)
            .value(ConditionValue::False)
            .reason("ReconciliationPassTimedOut")
            .message(format!("standing convoy ensure reconciliation exceeded its {timeout:?} pass timeout"))
            .observed_at(Utc::now())
            // Admission scan backlog does not prevent existing forge reads.
            .blocks_forge_observation(false)
            .build();
        self.failures.lock().expect("runtime health lock poisoned").insert(CONVOY_ENSURE_CONDITION_TYPE.to_string(), condition);
    }

    pub(super) fn clear_convoy_ensure_timeout(&self) {
        self.failures.lock().expect("runtime health lock poisoned").remove(CONVOY_ENSURE_CONDITION_TYPE);
    }

    pub(super) async fn conditions(&self) -> Vec<HostCondition> {
        let mut conditions = self.failures.lock().expect("runtime health lock poisoned").values().cloned().collect::<Vec<_>>();
        conditions.extend(self.issue_polling.condition());
        conditions.extend(store_read_retry_condition(&flotilla_store::load_metrics::read_retry_snapshot()));
        if let Some(state_dir) = self.restart_history_dir.clone() {
            let frequency =
                flotilla_provider_runtime::probe::blocking("restart history", flotilla_provider_runtime::probe::PROBE_TIMEOUT, move || {
                    crate::restart_history::recent_abnormal_restarts(state_dir.as_path(), Utc::now())
                })
                .await;
            let condition = match frequency {
                Ok(frequency) if frequency.count > 0 => Some(
                    HostCondition::builder()
                        .condition_type("Daemon/AbnormalRestarts")
                        .value(ConditionValue::False)
                        .reason("AbnormalExitFrequency")
                        .message(format!(
                            "daemon restarted {}× after abnormal exits in {}m",
                            frequency.count,
                            frequency.window.as_secs() / 60
                        ))
                        .observed_at(Utc::now())
                        .blocks_readiness(false)
                        // Historical restarts do not disable a currently live forge reader.
                        .blocks_forge_observation(false)
                        .build(),
                ),
                Ok(_) => None,
                Err(error) => Some(
                    HostCondition::builder()
                        .condition_type("Daemon/RestartTracking")
                        .value(ConditionValue::False)
                        .reason("RestartHistoryUnavailable")
                        .message(error)
                        .observed_at(Utc::now())
                        // Restart-history diagnostics are independent of forge I/O.
                        .blocks_forge_observation(false)
                        .build(),
                ),
            };
            conditions.extend(condition);
        }
        conditions
    }
}

pub(super) struct AgentAdapterCapabilityAssessment {
    pub(super) baseline: BTreeSet<String>,
    pub(super) regression: Option<HostCondition>,
}

pub(super) fn assess_agent_adapter_capabilities(
    previous: Option<&HostStatus>,
    current: &BTreeSet<String>,
    health: &DaemonHealthIdentity,
) -> AgentAdapterCapabilityAssessment {
    let Some(previous) = previous else {
        return AgentAdapterCapabilityAssessment { baseline: current.clone(), regression: None };
    };
    let baseline = match &previous.agent_adapter_baseline {
        Some(baseline) => baseline.clone(),
        None => match previous.agent_adapters() {
            Ok(adapters) => adapters,
            Err(error) => {
                warn!(%error, "cannot compare agent adapter capabilities with previous daemon generation");
                return AgentAdapterCapabilityAssessment { baseline: current.clone(), regression: None };
            }
        },
    };
    let same_daemon = previous.daemon_generation == health.generation && previous.daemon_started_at == Some(health.started_at);
    if same_daemon {
        return AgentAdapterCapabilityAssessment { baseline, regression: None };
    }
    let missing = baseline.difference(current).cloned().collect::<Vec<_>>();
    if missing.is_empty() {
        return AgentAdapterCapabilityAssessment { baseline: current.clone(), regression: None };
    }

    warn!(
        previous_generation = ?previous.daemon_generation,
        current_generation = ?health.generation,
        baseline_adapters = ?baseline,
        current_adapters = ?current,
        missing_adapters = ?missing,
        "host capabilities regressed across daemon restart"
    );
    let regression = Some(
        HostCondition::builder()
            .condition_type("CapabilityRegression")
            .value(ConditionValue::False)
            .reason("AgentAdaptersMissing")
            .message(format!("agent adapters from the last non-regressed daemon generation are missing: {}", missing.join(", ")))
            .observed_at(Utc::now())
            // Missing agent adapters affect crew placement, not forge reads.
            .blocks_forge_observation(false)
            .build(),
    );
    AgentAdapterCapabilityAssessment { baseline, regression }
}

#[cfg(test)]
pub(super) fn test_health_identity() -> DaemonHealthIdentity {
    DaemonHealthIdentity {
        generation: Some("test-generation".to_string()),
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: Utc::now(),
    }
}

pub(super) fn spawn_projection_parity_task(
    backend: ResourceBackend,
    namespace: String,
    projection: AggregatorProjectionState,
    runtime_health: RuntimeHealth,
    interval: Duration,
) -> JoinHandle<()> {
    spawn_periodic_task(interval, PeriodicTaskStart::AfterInterval, move || {
        let backend = backend.clone();
        let namespace = namespace.clone();
        let projection = projection.clone();
        let runtime_health = runtime_health.clone();
        async move {
            match projection_parity_condition(&backend, &namespace, &projection, &SystemClock).await {
                Ok(condition) => runtime_health.report_projection_parity(condition),
                Err(error) => warn!(%error, "failed to evaluate aggregator projection parity"),
            }
        }
    })
}

// Local convoy creation and aggregator watch delivery are not atomic. Give
// newly authored rows a small, non-renewing grace period; rows still missing
// at the boundary retain the existing host-readiness failure (#2736).
pub(super) const PROJECTION_PARITY_GRACE: chrono::Duration = chrono::Duration::seconds(10);

pub(super) async fn projection_parity_condition(
    backend: &ResourceBackend,
    namespace: &str,
    projection: &AggregatorProjectionState,
    clock: &dyn flotilla_resources::Clock,
) -> Result<Option<HostCondition>, String> {
    let stored = backend.using::<Convoy>(namespace).list().await.map_err(|error| error.to_string())?;
    let projected = match projection.local_result_set().await.rows {
        Rows::Convoys { rows, .. } => rows.into_iter().map(|row| row.resource.name).collect::<BTreeSet<_>>(),
        rows => return Err(format!("local convoy projection returned unexpected rows: {rows:?}")),
    };
    let now = clock.now();
    let expected = stored.items.iter().map(|convoy| convoy.metadata.name.clone()).collect::<BTreeSet<_>>();
    // `using` lists only this root's durable rows, never replicas. Creation time
    // (rather than last update or first parity observation) bounds the grace
    // even if controllers keep updating the row or the daemon restarts.
    // Sort missing names for stable diagnostics, independent of backend list order.
    let missing = stored
        .items
        .into_iter()
        .filter_map(|convoy| {
            let age = now.signed_duration_since(convoy.metadata.creation_timestamp);
            let within_grace = age >= chrono::Duration::zero() && age < PROJECTION_PARITY_GRACE;
            (!projected.contains(&convoy.metadata.name) && !within_grace).then_some(convoy.metadata.name)
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(None);
    }
    // Totals include every row in each snapshot; the missing list omits rows
    // still within grace, preserving the existing diagnostic counts.
    let message = format!(
        "durable store has {} convoys but the local aggregator projection has {}; missing: {}",
        expected.len(),
        projected.len(),
        missing.join(", ")
    );
    error!(
        stored = expected.len(),
        projected = projected.len(),
        missing = ?missing,
        "aggregator projection parity check failed"
    );
    Ok(Some(
        HostCondition::builder()
            .condition_type(PROJECTION_PARITY_CONDITION)
            .value(ConditionValue::False)
            .reason(LOCAL_ROWS_MISSING_REASON)
            .message(message)
            .observed_at(now)
            // Local projection lag blocks admission, not durable forge observation.
            .blocks_forge_observation(false)
            .build(),
    ))
}

pub(super) async fn apply_host_heartbeat_with_credentials(
    daemon: &Arc<InProcessDaemon>,
    namespace: &str,
    profile: &LocalProvisioningProfile,
    credential_store: Option<&CredentialStore>,
    health: &DaemonHealthIdentity,
    runtime_health: &RuntimeHealth,
) -> Result<(), String> {
    daemon.set_admission_free_space_path(PathBuf::from(&profile.repo_default_dir));
    ensure_host_exists(&daemon.resource_backend(), namespace, &profile.host_id, &profile.display_name).await?;
    let backend = daemon.resource_backend();
    let hosts = backend.using::<Host>(namespace);
    let host = hosts.get(&profile.host_id).await.map_err(|err| err.to_string())?;
    let adapter_assessment = assess_agent_adapter_capabilities(host.status.as_ref(), &profile.available_agent_adapters, health);
    if let Some(condition) = adapter_assessment.regression {
        runtime_health.report_capability_regression(condition);
    }
    let summary = daemon.local_host_description().await;
    let resource_store = backend.diagnostics().await.map_err(|err| err.to_string())?;
    if let Some(diagnostics) = resource_store.as_ref().filter(|diagnostics| !diagnostics.warnings.is_empty()) {
        warn!(
            event_count = diagnostics.event_count,
            object_count = diagnostics.object_count,
            resource_stream_count = diagnostics.resource_stream_count,
            max_retained_events = diagnostics.max_retained_events,
            warnings = ?diagnostics.warnings,
            "resource event log tripwire triggered",
        );
    }
    let (held_credentials, credential_expiry) = match credential_store {
        Some(store) => (store.held_credentials().await?, store.credential_expiry().await),
        None => (BTreeSet::new(), BTreeMap::new()),
    };
    let (disk_free_bytes, disk_probe_error) = match daemon.admission_free_space_bytes().await {
        Ok(bytes) => (bytes, None),
        Err(error) => (None, Some(error)),
    };
    let admission_free_space_floor_bytes = daemon.admission_free_space_floor_bytes()?;
    migrate_live_placement_policies(&backend, namespace, &profile.host_id, std::env::consts::OS).await?;
    let mut conditions = runtime_health.conditions().await;
    conditions.extend(daemon.project_image_layer_conditions().await);
    conditions.extend(daemon.observation_credential_conditions().await);
    if let Some(error) = disk_probe_error {
        conditions.push(
            HostCondition::builder()
                .condition_type("Filesystem/AvailableSpace")
                .value(ConditionValue::False)
                .reason("ProbeFailed")
                .message(error)
                .observed_at(Utc::now())
                // An admission free-space probe failure does not establish forge-read failure.
                .blocks_forge_observation(false)
                .build(),
        );
    }
    let build_records = backend.using::<flotilla_resources::ImageBuild>(namespace).list().await.map_err(|error| error.to_string())?;
    let lifecycles = daemon.lifecycle_reads();
    let local_host = profile.host_id.clone();
    conditions.extend(
        tokio::task::spawn_blocking(move || {
            let mut conditions = Vec::new();
            for build in &build_records.items {
                if build.spec.current().is_some_and(|spec| spec.host_ref == local_host) {
                    conditions.extend(image_build_failure_condition(build, lifecycles.as_ref()));
                }
            }
            conditions
        })
        .await
        .map_err(|error| error.to_string())?,
    );

    conditions.extend(file_descriptor_pressure_condition());
    if let Some(condition) = daemon.cleat_build_skew_condition().await {
        conditions.push(condition);
    }
    if let Some(condition) = resource_decode_quarantine_condition(resource_store.as_ref()) {
        conditions.push(condition);
    }
    if let Some(condition) = resource_field_ownership_condition(resource_store.as_ref()) {
        conditions.push(condition);
    }
    if let Some(condition) = resource_authorship_collision_condition(daemon, namespace).await? {
        conditions.push(condition);
    }
    if let Some(condition) = resource_replication_content_condition(daemon, namespace).await? {
        conditions.push(condition);
    }
    if let Some(condition) = daemon_forgejo_credential_condition(daemon, namespace).await {
        conditions.push(condition);
    }
    let mut capabilities = host_capabilities(&summary, profile, &held_credentials, &credential_expiry);
    if let Some(providers) = runtime_health.host_observations.provider_advertisements().await {
        capabilities.insert(flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.into(), serde_json::json!(providers));
    }
    if let Some(snapshot) = crate::resource_limits::io_pressure_snapshot().await {
        capabilities.insert("io_pressure".to_string(), snapshot);
    }
    capabilities.insert("store_read_retries".into(), json!(flotilla_store::load_metrics::read_retry_snapshot()));
    capabilities.insert("forge_budgets".into(), serde_json::to_value(daemon.forge_budget_rows()).expect("forge budgets serialize"));
    let ready = !conditions.iter().any(HostCondition::blocks_readiness);
    flotilla_store::apply_status_patch(
        &hosts,
        &profile.host_id,
        &HostStatusPatch::Heartbeat {
            description: Some(Box::new(summary)),
            capabilities,
            agent_adapter_baseline: Some(adapter_assessment.baseline),
            heartbeat_at: daemon.clock().now(),
            ready,
            resource_store: resource_store.map(Box::new),
            daemon_rss_bytes: flotilla_projections::host_summary::daemon_rss_bytes(),
            daemon_generation: health.generation.clone(),
            protocol_fingerprint: Some(flotilla_protocol::PROTOCOL_FINGERPRINT.to_string()),
            daemon_version: Some(health.version.clone()),
            daemon_started_at: Some(health.started_at),
            disk_free_bytes,
            admission_free_space_floor_bytes: Some(admission_free_space_floor_bytes),
            conditions,
            folded: Box::new(FoldedHostObservations {
                blob_sync: runtime_health.host_observations.blob_sync().await,
                fulfilment_refresh: runtime_health.host_observations.fulfilment_facts().await.unwrap_or_default(),
            }),
        },
    )
    .await
    .map_err(|err| err.to_string())?;
    Ok(())
}

/// Diagnose host-local intent against namespace-scoped declarations, never checkout observations.
/// Recompute on every heartbeat so late definitions and config edits clear the advisory.
pub(super) async fn daemon_forgejo_credential_condition(daemon: &Arc<InProcessDaemon>, namespace: &str) -> Option<HostCondition> {
    match resolve_daemon_forgejo_credential_condition(daemon, namespace).await {
        Ok(condition) => condition,
        Err(error) => {
            warn!(%error, %namespace, "could not diagnose daemon Forgejo credential mappings; retrying next heartbeat");
            None
        }
    }
}

pub(super) async fn resolve_daemon_forgejo_credential_condition(
    daemon: &Arc<InProcessDaemon>,
    namespace: &str,
) -> Result<Option<HostCondition>, String> {
    // Deliberately re-read config and scan current definitions on each heartbeat:
    // config corrections and new declarations must clear the advisory without restart.
    let config = daemon.config_store().load_daemon_config()?;
    if config.credentials.forgejo.is_empty() {
        return Ok(None);
    }
    let backend = daemon.resource_backend();
    // A configured manifest source may not have completed even its first pass.
    // Its status is published only after the directory has been processed.
    if let Some(manifests) = &config.manifests {
        let name = manifest_root_name(&manifests.reconciler_root, &manifests.dir, &manifests.source);
        match backend.including_replicas::<ManifestRoot>(namespace).get(&name).await {
            Ok(root) if root.object.status.is_some() => {}
            Ok(_) | Err(ResourceError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.to_string()),
        }
    }
    // A cursor for some other kind does not establish that Forge definitions
    // have arrived. Wait for each connected peer's Forge snapshot bookmark.
    for peer in daemon.connected_peer_node_ids().await {
        if backend.replica_writer::<Forge>(peer, namespace).cursor().await.map_err(|error| error.to_string())?.is_none() {
            return Ok(None);
        }
    }
    // The host-local map applies to source-addressed requests in any namespace.
    // A declaration outside the runtime namespace is still a valid use of a key.
    let mut namespaces =
        backend.stored_namespaces::<Forge>().await.map_err(|error| error.to_string())?.into_iter().collect::<BTreeSet<_>>();
    namespaces.insert(namespace.to_string());
    let mut declared = BTreeSet::new();
    for declared_namespace in &namespaces {
        let forges = backend.definitions::<Forge>(declared_namespace).list().await.map_err(|error| error.to_string())?;
        declared.extend(forges.into_iter().map(|forge| forge.spec.forge_id));
    }
    let unresolved = config.credentials.forgejo.keys().filter(|id| !declared.contains(*id)).map(|id| format!("`{id}`")).collect::<Vec<_>>();
    if unresolved.is_empty() {
        return Ok(None);
    }
    let namespace_context = namespaces.iter().map(|namespace| format!("`{namespace}`")).collect::<Vec<_>>().join(", ");
    Ok(Some(HostCondition::builder()
        .condition_type("DaemonForgejoCredentials")
        .value(ConditionValue::False)
        .reason("UnresolvedForgeMappings")
        .message(format!(
            "daemon.toml [credentials.forgejo] keys {} do not resolve to declared Forge resources in namespaces {namespace_context}; check the Forge IDs or declare the missing Forges",
            unresolved.join(", ")
        ))
        .observed_at(Utc::now())
        .blocks_readiness(false)
        // An unresolved mapping is source-specific; providers report their own credential errors.
        .blocks_forge_observation(false)
        .build()))
}

pub(super) async fn resource_authorship_collision_condition(
    daemon: &Arc<InProcessDaemon>,
    namespace: &str,
) -> Result<Option<HostCondition>, String> {
    let collisions = home_bound_authorship_collisions(&daemon.resource_backend(), namespace).await.map_err(|error| error.to_string())?;
    if collisions.is_empty() {
        return Ok(None);
    }
    let identities = collisions
        .iter()
        .map(|collision| {
            format!(
                "{}/{}/{} is authored locally at {} and replicated from {}",
                collision.kind, collision.namespace, collision.name, collision.local_root, collision.replica_root
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let vessel_guidance = if collisions.iter().any(|collision| collision.kind == Vessel::API_PATHS.kind) {
        " For a Vessel with live crew on the non-origin copy, preserve both copies until that crew is drained or finished; deleting the origin Vessel can tear down the placed actuator. Then remove the non-origin authored copy and let the origin's Vessel replicate and project again. Do not delete a running copy to clear this warning."
    } else {
        ""
    };
    Ok(Some(
        HostCondition::builder()
            .condition_type("ResourceReplication/AuthorshipCollision")
            .value(ConditionValue::False)
            .reason("HomeBoundRecordAuthoredAtMultipleRoots")
            .message(format!(
                "{} home-bound resource authorship collision{} detected: {identities}. Choose the record's natural home and delete the other authored copy at its root; the replicator will not choose a winner.{vessel_guidance}",
                collisions.len(),
                if collisions.len() == 1 { "" } else { "s" },
            ))
            .observed_at(Utc::now())
            .blocks_readiness(false)
            // A home-bound actuator collision does not disable forge reads.
            .blocks_forge_observation(false)
            .build(),
    ))
}

pub(super) async fn resource_replication_content_condition(
    daemon: &Arc<InProcessDaemon>,
    namespace: &str,
) -> Result<Option<HostCondition>, String> {
    let connected_peers = daemon.connected_peer_node_ids().await;
    if connected_peers.is_empty() {
        return Ok(None);
    }

    let backend = daemon.resource_backend();
    let mut peers_without_cursors = Vec::new();
    for peer in connected_peers {
        let mut cursor_count = 0;
        for kind in REGISTERED_RESOURCE_KINDS {
            if kind.replication_class == ReplicationClass::None {
                continue;
            }
            if flotilla_store::replica_cursor_for_resource_kind(&backend, namespace, kind.kind, &peer)
                .await
                .map_err(|error| error.to_string())?
                .is_some()
            {
                cursor_count += 1;
            }
        }
        if cursor_count == 0 {
            peers_without_cursors.push(peer);
        }
    }
    if peers_without_cursors.is_empty() {
        return Ok(None);
    }

    Ok(Some(
        HostCondition::builder()
            .condition_type("ResourceReplication")
            .value(ConditionValue::False)
            .reason("ReplicaCursorsMissing")
            .message(format!(
                "connected peer{} {} {} zero replica cursors; resource replication has not bootstrapped",
                if peers_without_cursors.len() == 1 { "" } else { "s" },
                peers_without_cursors.iter().map(NodeId::as_str).collect::<Vec<_>>().join(", "),
                if peers_without_cursors.len() == 1 { "has" } else { "have" },
            ))
            .observed_at(Utc::now())
            // Replication bootstrap lag does not disable local reads; lag handover belongs to #3414.
            .blocks_forge_observation(false)
            .build(),
    ))
}

pub(super) fn resource_decode_quarantine_condition(
    diagnostics: Option<&flotilla_resources::ResourceStoreDiagnostics>,
) -> Option<HostCondition> {
    let diagnostics = diagnostics?;
    let object_quarantines = &diagnostics.decode_quarantines;
    let event_quarantines = &diagnostics.event_decode_quarantines;
    if object_quarantines.is_empty() && event_quarantines.is_empty() {
        return None;
    }
    // Extend this list when source_owner gains another stored election input
    // or ForgeReads gains another demand/result kind; keep its scope test aligned.
    let forge_kinds = [
        Host::API_PATHS.kind,
        Forge::API_PATHS.kind,
        flotilla_resources::Project::API_PATHS.kind,
        flotilla_resources::Repository::API_PATHS.kind,
        flotilla_resources::ForgeRead::API_PATHS.kind,
        flotilla_resources::ForgeReadHeartbeat::API_PATHS.kind,
        flotilla_resources::Issue::API_PATHS.kind,
        flotilla_resources::ChangeRequest::API_PATHS.kind,
    ];
    let observation_corruption = object_quarantines
        .iter()
        .map(|record| record.kind.as_str())
        .chain(event_quarantines.iter().map(|record| record.kind.as_str()))
        .any(|kind| forge_kinds.contains(&kind));
    let identities = object_quarantines
        .iter()
        .map(|quarantine| format!("{}/{}: {}", quarantine.kind, quarantine.name, quarantine.error))
        .chain(
            event_quarantines
                .iter()
                .map(|quarantine| format!("{}/{}@{}: {}", quarantine.kind, quarantine.name, quarantine.event_version, quarantine.error)),
        )
        .collect::<Vec<_>>()
        .join("; ");
    let (reason, message) = if event_quarantines.is_empty() {
        (
            "StoredObjectDecodeFailed",
            format!(
                "{} stored resource object{} quarantined after typed decode failure{}: {identities}",
                object_quarantines.len(),
                if object_quarantines.len() == 1 { "" } else { "s" },
                if object_quarantines.len() == 1 { "" } else { "s" },
            ),
        )
    } else if object_quarantines.is_empty() {
        (
            "StoredEventDecodeFailed",
            format!(
                "{} stored resource event{} quarantined after typed decode failure{}: {identities}",
                event_quarantines.len(),
                if event_quarantines.len() == 1 { "" } else { "s" },
                if event_quarantines.len() == 1 { "" } else { "s" },
            ),
        )
    } else {
        (
            "StoredResourceDecodeFailed",
            format!(
                "{} stored resource object{} and {} event{} quarantined after typed decode failures: {identities}",
                object_quarantines.len(),
                if object_quarantines.len() == 1 { "" } else { "s" },
                event_quarantines.len(),
                if event_quarantines.len() == 1 { "" } else { "s" },
            ),
        )
    };
    Some(
        HostCondition::builder()
            .condition_type("ResourceStore/DecodeQuarantine")
            .value(ConditionValue::False)
            .reason(reason)
            .message(message)
            .observed_at(Utc::now())
            // Only corrupt forge inputs/results disable observation; unrelated actuator records do not.
            .blocks_forge_observation(observation_corruption)
            .build(),
    )
}

pub(super) fn resource_field_ownership_condition(
    diagnostics: Option<&flotilla_resources::ResourceStoreDiagnostics>,
) -> Option<HostCondition> {
    let violations = &diagnostics?.field_ownership_violations;
    if violations.is_empty() {
        return None;
    }
    let details = violations
        .iter()
        .map(|violation| {
            format!(
                "{}/{}/{} {:?} attempted {}={} ({})",
                violation.kind,
                violation.namespace,
                violation.name,
                violation.writer.role,
                violation.field,
                violation.attempted_value,
                violation.rule
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Some(
        HostCondition::builder()
            .condition_type("ResourceStore/FieldOwnership")
            .value(ConditionValue::False)
            .reason("FieldOwnershipViolation")
            .message(format!(
                "{} field ownership violation{} recorded: {details}",
                violations.len(),
                if violations.len() == 1 { "" } else { "s" }
            ))
            .observed_at(Utc::now())
            .blocks_readiness(false)
            // Historical rejected writes do not disable current forge reads.
            .blocks_forge_observation(false)
            .build(),
    )
}

pub(super) fn host_capabilities(
    summary: &HostSummary,
    profile: &LocalProvisioningProfile,
    held_credentials: &BTreeSet<String>,
    credential_expiry: &BTreeMap<String, CredentialExpiry>,
) -> BTreeMap<String, serde_json::Value> {
    BTreeMap::from([
        (flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.to_string(), json!(profile.environment_providers)),
        (AGENT_ADAPTERS_CAPABILITY.to_string(), json!(profile.available_agent_adapters)),
        (HELD_CREDENTIALS_CAPABILITY.to_string(), json!(held_credentials)),
        (CREDENTIAL_EXPIRY_CAPABILITY.to_string(), json!(credential_expiry)),
        ("docker".to_string(), json!(profile.docker_available)),
        ("os".to_string(), json!(summary.system.os)),
        ("terminal_pools".to_string(), json!(profile.available_pools)),
    ])
}

/// A long reconcile also stalls other names, so it counts as missing loop progress.
pub(super) fn spawn_controller_loop_watchdog(
    name: &'static str,
    heartbeat: Arc<AtomicU64>,
    interval: Duration,
    health: RuntimeHealth,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await;
        let mut last_value = heartbeat.load(Ordering::Relaxed);
        let mut last_progress = tokio::time::Instant::now();
        loop {
            ticker.tick().await;
            let value = heartbeat.load(Ordering::Relaxed);
            if value != last_value {
                last_value = value;
                last_progress = tokio::time::Instant::now();
                health.clear_controller_loop_stall(name);
            } else if last_progress.elapsed() >= interval.saturating_mul(2) {
                health.report_controller_loop_stall(name, last_progress.elapsed());
            }
        }
    })
}

#[cfg(test)]
mod provider_readiness_tests {
    use super::*;
    use flotilla_protocol::provider::{ProviderAdvertisement, ProviderReadiness};

    // Re-observation publishes credential loss, and an expired sample never
    // keeps a cloud instance ready. Generate both sides of the freshness boundary.
    #[hegel::test]
    fn refreshed_and_expired_provider_readiness_fails_closed(tc: hegel::TestCase) {
        let age = tc.draw(hegel::generators::integers::<u64>().min_value(59).max_value(61));
        tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap().block_on(async {
            let observations = HostObservations::default();
            let mut provider =
                ProviderAdvertisement { instance: "cloud".into(), readiness: ProviderReadiness::Ready, ..Default::default() };
            observations.record_provider_advertisements(vec![provider.clone()]).await;
            tokio::time::advance(Duration::from_secs(age)).await;
            let current = observations.provider_advertisements().await.unwrap();
            assert_eq!(current[0].readiness == ProviderReadiness::Ready, age < 60);
            provider.readiness = ProviderReadiness::MissingCredential { reason: "account token removed".into() };
            observations.record_provider_advertisements(vec![provider.clone()]).await;
            assert_eq!(observations.provider_advertisements().await.unwrap()[0].readiness, provider.readiness);
        });
    }
}

/// Informational pressure signal, independent of readiness, forge admission,
/// and restart policy. Recomputed on every heartbeat.
fn store_read_retry_condition(snapshot: &BTreeMap<&str, flotilla_store::load_metrics::ReadRetrySnapshot>) -> Option<HostCondition> {
    let prolonged = snapshot
        .iter()
        .filter(|(_, metrics)| metrics.prolonged > 0)
        .map(|(class, metrics)| {
            format!(
                "{class}: {} prolonged, {} consecutive busy refusals, oldest {} ms",
                metrics.prolonged, metrics.consecutive, metrics.oldest_millis
            )
        })
        .collect::<Vec<_>>();
    if prolonged.is_empty() {
        return None;
    }
    Some(
        HostCondition::builder()
            .condition_type("ResourceStore/ReadRetries")
            .value(ConditionValue::False)
            .reason("StoreBusyReadRetriesProlonged")
            .message(format!(
                "store-busy reads retrying for at least {} seconds; {}",
                flotilla_store::load_metrics::READ_RETRY_PROLONGED.as_secs(),
                prolonged.join("; ")
            ))
            .observed_at(Utc::now())
            .blocks_readiness(false)
            .blocks_forge_observation(false)
            .build(),
    )
}

#[cfg(test)]
mod read_retry_health_tests {
    use super::*;
    use flotilla_store::load_metrics::ReadRetrySnapshot;

    // The health condition exists only while at least one class has prolonged
    // retries. Historical counters never keep it latched after recovery.
    #[hegel::test]
    fn prolonged_retries_are_informational_and_recovery_clears(tc: hegel::TestCase) {
        let prolonged = tc.draw(hegel::generators::integers::<u64>().min_value(0).max_value(10));
        let historical = tc.draw(hegel::generators::integers::<u64>());
        let mut snapshot = BTreeMap::from([(
            "controller",
            ReadRetrySnapshot { prolonged, retries: historical, recoveries: historical, ..Default::default() },
        )]);
        let condition = store_read_retry_condition(&snapshot);
        assert_eq!(condition.is_some(), prolonged > 0);
        if let Some(condition) = condition {
            assert_eq!(condition.condition_type, "ResourceStore/ReadRetries");
            assert!(!condition.blocks_readiness());
            assert!(!condition.blocks_forge_observation());
            assert!(condition.message.contains("controller"));
        }
        snapshot.get_mut("controller").unwrap().prolonged = 0;
        assert!(store_read_retry_condition(&snapshot).is_none());
    }
}

// Heartbeats expose engine failure causes, suppress stale failures, and retain checked-read errors.
#[cfg(test)]
#[test]
fn image_health_follows_engine_without_a_copy() {
    use flotilla_provisioning::engine::image_build::ImageBuildBinding;
    use flotilla_provisioning_testkit::{image_build, Ending, LifecycleFixture};
    let build = image_build(flotilla_resources::ImageBuildPhase::Failed);
    let fixture = LifecycleFixture::<ImageBuildBinding>::new(false);
    fixture.request(&build);
    assert!(image_build_failure_condition(&build, fixture.reads.as_ref()).is_none());
    fixture.finish(&build, Ending::GaveUp("engine cause"));
    let condition = image_build_failure_condition(&build, fixture.reads.as_ref()).expect("engine failure");
    assert_eq!(condition.reason, "ImageBuildFailed");
    assert_eq!(condition.message, "engine cause");
    assert!(!condition.blocks_readiness);
    assert!(!condition.blocks_forge_observation);
    fixture.poison();
    let condition = image_build_failure_condition(&build, fixture.reads.as_ref()).expect("checked read failure");
    assert_eq!(condition.reason, "LifecycleReadFailed");
    assert!(condition.message.contains("poisoned"));
}
