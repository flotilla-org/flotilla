//! Runtime health and heartbeat conditions.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use chrono::Utc;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_projections::IssuePollingHealth;

use flotilla_credentials::CredentialStore;
use flotilla_protocol::HostSummary;
use flotilla_resources::ResourceError;
use flotilla_resources::{
    ConditionValue, CredentialExpiry, Forge, FulfilmentFacts, Host, HostCondition, HostStatus, HostStatusPatch, ResourceObject,
    AGENT_ADAPTERS_CAPABILITY, CREDENTIAL_EXPIRY_CAPABILITY, HELD_CREDENTIALS_CAPABILITY,
};
use flotilla_store::{Listed, TypedResolver};
use serde_json::json;
use tracing::warn;

use super::discovery::LocalProvisioningProfile;
use super::seed::{canonicalize_kinds_for, ensure_host_matches};
use super::tasks::{AGGREGATOR_CONTROLLER, CONVOY_ENSURE_CONDITION_TYPE, RETENTION_CONDITION_TYPE};
use crate::{
    resource_limits::file_descriptor_pressure_condition, resource_manifest::manifest_root_name, supervisor::RestartBudgetExhausted,
};

const FLEET_DESIGNATION_CONDITION_TYPE: &str = "Message/FleetDesignation";

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
    /// Wakes the heartbeat to publish a condition straight away, before its next tick.
    pub(super) heartbeat_now: Arc<tokio::sync::Notify>,
    /// What the heartbeat last published, and the Hosts it last listed.
    pub(super) heartbeat: Arc<tokio::sync::Mutex<HeartbeatMemory>>,
    /// Where the heartbeat publishes; the store's, unless a test counts the calls.
    pub(super) heartbeat_publish: HeartbeatPublish,
}

/// Latest periodic observations of the local Host that the heartbeat and the fulfilment probe
/// share. Host status is "latest value only" (WORKLOAD.md).
#[derive(Clone, Default)]
pub(super) struct HostObservations {
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
        formatter.debug_struct("HostObservations").finish_non_exhaustive()
    }
}

impl HostObservations {
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

    /// Names the FleetDesignation message delivery waits for, or clears the wait.
    pub(super) fn report_delivery_wait(&self, wait: Option<String>) {
        let mut failures = self.failures.lock().expect("runtime health lock poisoned");
        match wait {
            Some(message) => {
                if failures.get(FLEET_DESIGNATION_CONDITION_TYPE).is_some_and(|existing| existing.message == message) {
                    return;
                }
                failures.insert(
                    FLEET_DESIGNATION_CONDITION_TYPE.to_string(),
                    HostCondition::builder()
                        .condition_type(FLEET_DESIGNATION_CONDITION_TYPE)
                        .value(ConditionValue::False)
                        .reason("FleetDesignationUnavailable")
                        .message(message)
                        .observed_at(Utc::now())
                        .blocks_readiness(false)
                        .blocks_forge_observation(false)
                        .build(),
                );
            }
            None => {
                failures.remove(FLEET_DESIGNATION_CONDITION_TYPE);
            }
        }
    }

    /// The archive's `Archive` condition (#3939): present while a bundle is overdue.
    pub(super) fn report_archive(&self, condition: Option<HostCondition>) {
        let mut failures = self.failures.lock().expect("runtime health lock poisoned");
        match condition {
            Some(condition) => {
                failures.insert(condition.condition_type.clone(), condition);
            }
            None => {
                failures.remove(super::archive::ARCHIVE_CONDITION);
            }
        }
    }

    /// Artifact retention's condition: present while its failures are past their retry limit.
    pub(super) fn report_retention(&self, failing: Option<String>) {
        let mut failures = self.failures.lock().expect("runtime health lock poisoned");
        match failing {
            Some(message) => {
                let condition = HostCondition::builder()
                    .condition_type(RETENTION_CONDITION_TYPE)
                    .value(ConditionValue::False)
                    .reason("RetentionFailing")
                    .message(message)
                    .observed_at(Utc::now())
                    .blocks_readiness(false)
                    .blocks_forge_observation(false)
                    .build();
                failures.insert(RETENTION_CONDITION_TYPE.to_string(), condition);
            }
            None => {
                failures.remove(RETENTION_CONDITION_TYPE);
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
    // The observed store's generation is minted per daemon process, so it alone tells a restart
    // from this daemon's own earlier heartbeat; the decided store keeps no start time.
    let same_daemon = match (&previous.daemon_generation, &health.generation) {
        (Some(previous), Some(current)) => previous == current,
        _ => previous.daemon_started_at == Some(health.started_at),
    };
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

pub(super) async fn apply_host_heartbeat_with_credentials(
    daemon: &Arc<InProcessDaemon>,
    namespace: &str,
    profile: &LocalProvisioningProfile,
    credential_store: Option<&CredentialStore>,
    health: &DaemonHealthIdentity,
    runtime_health: &RuntimeHealth,
) -> Result<(), String> {
    daemon.set_admission_free_space_path(PathBuf::from(&profile.repo_default_dir));
    let backend = daemon.resource_backend();
    let hosts = backend.clone().using::<Host>(namespace);
    let mut memory = runtime_health.heartbeat.lock().await;
    // This daemon's own Host, and those its kinds may name, listed again only once they moved:
    // after bringing its own in line with the profile, among other writes.
    let mut listed = memory.hosts.read(&hosts).await.map_err(|err| err.to_string())?;
    let own = |host: &&ResourceObject<Host>| host.metadata.name == profile.host_id;
    let existing = listed.iter().find(own);
    if ensure_host_matches(&hosts, existing, &profile.host_id, &profile.display_name, profile.max_vessels).await? {
        listed = memory.hosts.read(&hosts).await.map_err(|err| err.to_string())?;
        // A Host made or remade holds none of what was published.
        memory.published = None;
    }
    let host = listed.iter().find(own).ok_or_else(|| format!("host {} is not held", profile.host_id))?;
    if host.status.is_none() {
        memory.published = None;
    }
    let adapter_assessment = assess_agent_adapter_capabilities(host.status.as_ref(), &profile.available_agent_adapters, health);
    if let Some(condition) = adapter_assessment.regression {
        runtime_health.report_capability_regression(condition);
    }
    let summary = daemon.local_host_description().await;
    let (held_credentials, credential_expiry) = match credential_store {
        Some(store) => (store.held_credentials().await?, store.credential_expiry().await),
        None => (BTreeSet::new(), BTreeMap::new()),
    };
    let (disk_free_bytes, disk_probe_error) = match daemon.admission_free_space_bytes().await {
        Ok(bytes) => (bytes, None),
        Err(error) => (None, Some(error)),
    };
    let below_free_space_floor = below_floor(disk_free_bytes, daemon.admission_free_space_floor_bytes()?);
    canonicalize_kinds_for(&backend, namespace, &listed, &profile.host_id).await?;
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
    conditions.extend(file_descriptor_pressure_condition());
    conditions.extend(daemon.generation_conditions().await);
    if let Some(condition) = daemon_forgejo_credential_condition(daemon, namespace).await {
        conditions.push(condition);
    }
    let report = HeartbeatReport {
        description: summary,
        providers: runtime_health
            .host_observations
            .provider_advertisements()
            .await
            .unwrap_or_else(|| profile.environment_providers.clone()),
        held_credentials,
        credential_expiry,
        agent_adapter_baseline: adapter_assessment.baseline,
        ready: !conditions.iter().any(HostCondition::blocks_readiness),
        daemon_generation: health.generation.clone(),
        below_free_space_floor,
        conditions,
    };
    // Unchanged since the last publish: no patch is made, and nothing is read or written.
    if memory.published.as_ref().is_some_and(|published| published.same_as(&report)) {
        return Ok(());
    }
    runtime_health
        .heartbeat_publish
        .0
        .publish(&hosts, &profile.host_id, &report.patch(profile, daemon.clock().now()))
        .await
        .map_err(|err| err.to_string())?;
    memory.published = Some(report);
    Ok(())
}

/// Where the heartbeat publishes its own Host's status.
#[async_trait::async_trait]
pub(super) trait HostStatusPublish: Send + Sync {
    async fn publish(&self, hosts: &TypedResolver<Host>, name: &str, patch: &HostStatusPatch) -> Result<(), ResourceError>;
}

/// Publishes through the store, which writes only an observation that changed.
struct StoreStatusPublish;

#[async_trait::async_trait]
impl HostStatusPublish for StoreStatusPublish {
    async fn publish(&self, hosts: &TypedResolver<Host>, name: &str, patch: &HostStatusPatch) -> Result<(), ResourceError> {
        flotilla_store::observe_status(hosts, name, patch).await
    }
}

#[derive(Clone)]
pub(super) struct HeartbeatPublish(pub(super) Arc<dyn HostStatusPublish>);

impl Default for HeartbeatPublish {
    fn default() -> Self {
        Self(Arc::new(StoreStatusPublish))
    }
}

impl std::fmt::Debug for HeartbeatPublish {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("HeartbeatPublish").finish_non_exhaustive()
    }
}

/// What the heartbeat keeps between its ticks.
#[derive(Default)]
pub(super) struct HeartbeatMemory {
    hosts: Listed<Host>,
    /// The report last published, which an unchanged tick does not publish again.
    published: Option<HeartbeatReport>,
}

impl std::fmt::Debug for HeartbeatMemory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("HeartbeatMemory").field("published", &self.published.is_some()).finish_non_exhaustive()
    }
}

/// Whether `free` bytes are below the admission `floor`: a floor of zero admits whatever is free,
/// and an unread free figure is unknown.
pub(super) fn below_floor(free: Option<u64>, floor: u64) -> Option<bool> {
    if floor == 0 {
        return Some(false);
    }
    free.map(|free| free < floor)
}

/// What one heartbeat observes of this daemon's own host, compared with the last one published
/// before any patch is made of it.
#[derive(Debug, Clone)]
pub(super) struct HeartbeatReport {
    description: HostSummary,
    providers: Vec<flotilla_protocol::provider::ProviderAdvertisement>,
    held_credentials: BTreeSet<String>,
    credential_expiry: BTreeMap<String, CredentialExpiry>,
    agent_adapter_baseline: BTreeSet<String>,
    ready: bool,
    daemon_generation: Option<String>,
    below_free_space_floor: Option<bool>,
    conditions: Vec<HostCondition>,
}

impl HeartbeatReport {
    /// Whether `other` observes what this does: each condition's observation time aside.
    fn same_as(&self, other: &Self) -> bool {
        let Self {
            description,
            providers,
            held_credentials,
            credential_expiry,
            agent_adapter_baseline,
            ready,
            daemon_generation,
            below_free_space_floor,
            conditions,
        } = self;
        *description == other.description
            && *providers == other.providers
            && *held_credentials == other.held_credentials
            && *credential_expiry == other.credential_expiry
            && *agent_adapter_baseline == other.agent_adapter_baseline
            && *ready == other.ready
            && *daemon_generation == other.daemon_generation
            && *below_free_space_floor == other.below_free_space_floor
            && conditions.len() == other.conditions.len()
            && conditions.iter().zip(&other.conditions).all(|(left, right)| left.same_observation(right))
    }

    fn patch(&self, profile: &LocalProvisioningProfile, heartbeat_at: chrono::DateTime<Utc>) -> HostStatusPatch {
        HostStatusPatch::Heartbeat {
            description: Some(Box::new(self.description.clone())),
            capabilities: BTreeMap::from([
                (flotilla_resources::ENVIRONMENT_PROVIDERS_CAPABILITY.to_string(), json!(self.providers)),
                (AGENT_ADAPTERS_CAPABILITY.to_string(), json!(profile.available_agent_adapters)),
                (HELD_CREDENTIALS_CAPABILITY.to_string(), json!(self.held_credentials)),
                (CREDENTIAL_EXPIRY_CAPABILITY.to_string(), json!(self.credential_expiry)),
            ]),
            agent_adapter_baseline: Some(self.agent_adapter_baseline.clone()),
            heartbeat_at,
            ready: self.ready,
            daemon_generation: self.daemon_generation.clone(),
            protocol_fingerprint: Some(flotilla_protocol::PROTOCOL_FINGERPRINT.to_string()),
            below_free_space_floor: self.below_free_space_floor,
            conditions: self.conditions.clone(),
        }
    }
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
        // A root's first pass leaves a revision or a source error behind.
        match daemon.manifest_roots().root(namespace, &name).await? {
            Some(root)
                if root.status.applied_revision.is_some() || root.status.source_error.is_some() || !root.status.documents.is_empty() => {}
            _ => return Ok(None),
        }
    }
    // Forge definitions arrive with each connected peer's decided log: wait until this store has
    // committed a first round of every one of them.
    let decided = daemon.decided_store()?;
    for peer in daemon.connected_peer_node_ids().await {
        if !decided.has_watermark(peer.as_str()) {
            return Ok(None);
        }
    }
    // The decided store holds definitions without namespaces.
    let namespaces = BTreeSet::from([namespace.to_string()]);
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
