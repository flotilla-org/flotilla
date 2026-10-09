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
use flotilla_aggregator::IssuePollingHealth;
use flotilla_core::{aggregator_projection::AggregatorProjectionState, in_process::InProcessDaemon};
use flotilla_credentials::CredentialStore;
use flotilla_protocol::{HostSummary, NodeId, Rows};
use flotilla_resources::Resource;
use flotilla_resources::{
    home_bound_authorship_collisions, ConditionValue, Convoy, CredentialExpiry, Forge, Host, HostCondition, HostStatus, HostStatusPatch,
    ManifestRoot, ReplicationClass, ResourceBackend, ResourceError, SystemClock, Vessel, AGENT_ADAPTERS_CAPABILITY,
    CREDENTIAL_EXPIRY_CAPABILITY, HELD_CREDENTIALS_CAPABILITY, REGISTERED_RESOURCE_KINDS,
};
use serde_json::json;
use tokio::task::JoinHandle;
use tracing::{error, warn};

use super::discovery::LocalProvisioningProfile;
use super::seed::{ensure_host_exists, migrate_live_placement_policies};
use super::tasks::{spawn_periodic_task, PeriodicTaskStart, CONVOY_ENSURE_CONDITION_TYPE};
use crate::{
    resource_limits::file_descriptor_pressure_condition, resource_manifest::manifest_root_name, supervisor::RestartBudgetExhausted,
};

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
}

impl RuntimeHealth {
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
            .build();
        self.failures.lock().expect("runtime health lock poisoned").insert(condition_type, condition);
    }

    pub(super) fn clear_controller_loop_stall(&self, name: &str) {
        self.failures.lock().expect("runtime health lock poisoned").remove(&format!("ControllerLoop/{name}"));
    }

    pub(super) fn report_projection_parity(&self, condition: Option<HostCondition>) {
        const CONDITION_TYPE: &str = "ProjectionParity";
        let mut failures = self.failures.lock().expect("runtime health lock poisoned");
        match condition {
            Some(condition) => {
                failures.insert(CONDITION_TYPE.to_string(), condition);
            }
            None => {
                failures.remove(CONDITION_TYPE);
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
            .build();
        self.failures.lock().expect("runtime health lock poisoned").insert(CONVOY_ENSURE_CONDITION_TYPE.to_string(), condition);
    }

    pub(super) fn clear_convoy_ensure_timeout(&self) {
        self.failures.lock().expect("runtime health lock poisoned").remove(CONVOY_ENSURE_CONDITION_TYPE);
    }

    pub(super) async fn conditions(&self) -> Vec<HostCondition> {
        let mut conditions = self.failures.lock().expect("runtime health lock poisoned").values().cloned().collect::<Vec<_>>();
        conditions.extend(self.issue_polling.condition());
        if let Some(state_dir) = self.restart_history_dir.clone() {
            let frequency = flotilla_core::probe::blocking("restart history", flotilla_core::probe::PROBE_TIMEOUT, move || {
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
            .condition_type("ProjectionParity")
            .value(ConditionValue::False)
            .reason("LocalRowsMissing")
            .message(message)
            .observed_at(now)
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
    if let Some(error) = disk_probe_error {
        conditions.push(
            HostCondition::builder()
                .condition_type("Filesystem/AvailableSpace")
                .value(ConditionValue::False)
                .reason("ProbeFailed")
                .message(error)
                .observed_at(Utc::now())
                .build(),
        );
    }
    let build_records = backend.using::<flotilla_resources::ImageBuild>(namespace).list().await.map_err(|error| error.to_string())?;
    for build in &build_records.items {
        if build.spec.host_ref == profile.host_id
            && build.status.as_ref().is_some_and(|status| status.phase == flotilla_resources::ImageBuildPhase::Failed)
            && !build_records.items.iter().any(|next| next.spec.previous_build_ref.as_deref() == Some(build.metadata.name.as_str()))
        {
            let reason =
                build.status.as_ref().and_then(|status| status.failure.as_ref()).map(|failure| failure.reason.clone()).unwrap_or_default();
            conditions.push(
                HostCondition::builder()
                    .condition_type(format!("ImageBuild/{}", build.metadata.name))
                    .value(ConditionValue::False)
                    .reason("ImageBuildFailed")
                    .message(reason)
                    .observed_at(Utc::now())
                    .blocks_readiness(false)
                    .build(),
            );
        }
    }

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
    if let Some(snapshot) = crate::resource_limits::io_pressure_snapshot().await {
        capabilities.insert("io_pressure".to_string(), snapshot);
    }
    capabilities.insert("forge_budgets".into(), serde_json::to_value(daemon.forge_budget_rows()).expect("forge budgets serialize"));
    let ready = !conditions.iter().any(HostCondition::blocks_readiness);
    flotilla_resources::apply_status_patch(
        &hosts,
        &profile.host_id,
        &HostStatusPatch::Heartbeat {
            description: Some(Box::new(summary)),
            capabilities,
            agent_adapter_baseline: Some(adapter_assessment.baseline),
            heartbeat_at: Utc::now(),
            ready,
            resource_store: resource_store.map(Box::new),
            daemon_rss_bytes: flotilla_core::host_summary::daemon_rss_bytes(),
            daemon_generation: health.generation.clone(),
            protocol_fingerprint: Some(flotilla_protocol::PROTOCOL_FINGERPRINT.to_string()),
            daemon_version: Some(health.version.clone()),
            daemon_started_at: Some(health.started_at),
            disk_free_bytes,
            admission_free_space_floor_bytes: Some(admission_free_space_floor_bytes),
            conditions,
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
            if flotilla_resources::replica_cursor_for_resource_kind(&backend, namespace, kind.kind, &peer)
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
