use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use flotilla_protocol::{
    CanonicalHostId, EnvironmentInfo, HostProviderStatus, HostSummary, SleepInhibitionHealth, AGENT_ADAPTER_PROVIDER_CATEGORY,
    TERMINAL_POOL_PROVIDER_CATEGORY,
};
use serde::{Deserialize, Serialize};

use crate::{
    checkout::ConditionValue, resource::define_resource, retention::ResourceStoreDiagnostics, status_patch::StatusPatch, EventHistory,
    ReplicationClass,
};

// Host status is a latest-value heartbeat observation (WORKLOAD.md
// `host_heartbeat`), not durable history: a lagging watcher relists the
// current Host rather than replaying superseded heartbeats.
define_resource!(
    Host,
    "hosts",
    HostSpec,
    HostStatus,
    HostStatusPatch,
    replication = ReplicationClass::HomeBoundRuntime,
    event_history = EventHistory::LatestOnly
);

/// Kind-neutral provider instance advertisements in the existing capability map.
pub const ENVIRONMENT_PROVIDERS_CAPABILITY: &str = "environment_providers";
pub const AGENT_ADAPTERS_CAPABILITY: &str = "agent_adapters";
pub const HELD_CREDENTIALS_CAPABILITY: &str = "held_credentials";
pub const CREDENTIAL_EXPIRY_CAPABILITY: &str = "credential_expiry";
/// Scope name under [`CREDENTIAL_EXPIRY_CAPABILITY`] for the host's ambient
/// claude login (the credentials file a `claude login` leaves behind), which
/// is held material without a `CredentialSpec` declaring it.
pub const AMBIENT_CLAUDE_CREDENTIAL_SCOPE: &str = "ambient:claude";
pub const TERMINAL_POOLS_CAPABILITY: &str = "terminal_pools";
pub const AGENTLESS_CAPABILITY: &str = "agentless";
pub const TRANSPORT_CAPABILITY: &str = "transport";
pub const PLACEMENT_CAPABILITY: &str = "placement";
pub const OWNING_DAEMON_CAPABILITY: &str = "owning_daemon";
pub const HEARTBEAT_READY_TTL_SECS: i64 = 60;
/// A heartbeat that observes nothing new apart from timestamps still commits
/// once the stored `heartbeat_at` is this old. Readers then see `heartbeat_at`
/// advance within `HEARTBEAT_REFRESH_SECS` plus one heartbeat interval, which
/// stays inside [`HEARTBEAT_READY_TTL_SECS`] for any interval under 40 s.
pub const HEARTBEAT_REFRESH_SECS: i64 = HEARTBEAT_READY_TTL_SECS / 3;
pub const SLEEP_INHIBITION_CONDITION_TYPE: &str = "SleepInhibition";

/// Resolve a user-authored host resource name or display-name alias to the
/// stable host resource identity used by comparison surfaces.
pub fn canonical_host_id<'a>(
    hosts: impl IntoIterator<Item = &'a crate::ResourceObject<Host>>,
    host_ref: &str,
) -> Result<Option<CanonicalHostId>, String> {
    let hosts = hosts.into_iter().collect::<Vec<_>>();
    if let Some(host) = hosts.iter().find(|host| host.metadata.name == host_ref) {
        return Ok(Some(CanonicalHostId::resolved(host.metadata.name.clone())));
    }
    let mut matching = hosts.iter().filter(|host| host.spec.display_name == host_ref);
    let Some(host) = matching.next() else {
        return Ok(None);
    };
    if matching.any(|candidate| candidate.metadata.name != host.metadata.name) {
        return Err(format!("host reference `{host_ref}` is ambiguous"));
    }
    Ok(Some(CanonicalHostId::resolved(host.metadata.name.clone())))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSpec {
    #[serde(default)]
    pub display_name: String,
    #[serde(default, skip_serializing_if = "HostConnection::is_daemon")]
    pub connection: HostConnection,
    /// Number of Rust crews this host is sized for; does not divide CPU
    /// capacity or build parallelism. Currently no admission or memory budget
    /// consumes it; retain the sizing metadata and stored shape for ADR 0047.
    /// If retired later, accept and drop it for one fleet roll before removing
    /// its decoder (rather than making old Host records undecodable).
    /// Previous-generation Host records omit it (ADR 0047); remove this
    /// decoder default one fleet roll after all hosts write the field.
    #[serde(default = "default_expected_concurrent_rust_crews")]
    pub expected_concurrent_rust_crews: u32,
    /// ADR 0047: absent means placement-host builds; retain default for one roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_build_capacity: Option<crate::ImageBuildCapacity>,
    /// The vessels an agentless SSH host declares it can hold (#3793). A
    /// capped host takes unpinned convoys; an uncapped one takes only pinned
    /// work. Previous-generation Host records omit it (ADR 0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_vessels: Option<u32>,
    /// An agentless SSH host declared dedicated and single-tenant (#4002):
    /// the whole VM is the sandbox boundary, so least-privilege placement
    /// ranks its host reach as contained. Previous-generation Host records
    /// omit it (ADR 0047).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub single_tenant: bool,
}

const fn default_expected_concurrent_rust_crews() -> u32 {
    4
}

impl Default for HostSpec {
    fn default() -> Self {
        Self {
            display_name: String::new(),
            connection: HostConnection::default(),
            expected_concurrent_rust_crews: 4,
            image_build_capacity: None,
            max_vessels: None,
            single_tenant: false,
        }
    }
}

impl HostSpec {
    /// The cap that bounds a placement of `realisation` here: every placement
    /// on an agentless SSH host (#3793), and Docker per-vessel placements on
    /// a daemon's host (#4002). `None` is unbounded, or pin-only on an
    /// agentless host.
    pub fn vessel_cap(&self, realisation: &crate::FulfilmentRealisation) -> Option<u32> {
        match (&self.connection, realisation) {
            (HostConnection::AgentlessSsh { .. }, _) | (_, crate::FulfilmentRealisation::DockerPerVessel { .. }) => self.max_vessels,
            _ => None,
        }
    }

    pub fn rust_build_jobs(&self, host_cores: usize) -> usize {
        host_cores.max(1)
    }
}

/// How the owning daemon reaches this Host for actuation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostConnection {
    #[default]
    Daemon,
    AgentlessSsh {
        owning_daemon: String,
        destination: String,
    },
}

impl HostConnection {
    pub fn is_daemon(&self) -> bool {
        matches!(self, Self::Daemon)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct HostStatus {
    /// Last descriptive observation from the owning daemon, useful even offline.
    /// `environments` holds every visible environment; `host_summary()` projects
    /// the provisioned-only list expected by the legacy summary surface.
    /// HostSummary and its nested types are persisted here: future shape changes
    /// must obey ADR 0047 and the stored-record golden corpus.
    /// ADR 0047: previous-generation records omit this; retain the default for one roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<HostSummary>,
    #[serde(default)]
    pub capabilities: BTreeMap<String, serde_json::Value>,
    /// Live observations keyed by FulfilmentKind name. A fact remains tied to
    /// the image or host binary version it was obtained from.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[builder(default)]
    pub fulfilment_facts: BTreeMap<String, FulfilmentFacts>,
    /// Durable cache and rate accounting for real-request model probes.
    #[serde(default, skip_serializing_if = "ModelProbeState::is_empty")]
    #[builder(default)]
    pub model_probes: ModelProbeState,
    /// Last adapter inventory that did not regress from the preceding
    /// generation. A regressed generation retains this baseline so another
    /// restart cannot silently absorb the loss.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_adapter_baseline: Option<BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub ready: bool,
    /// A host may be reachable while its scheduled sleep prevents admission.
    /// Absent in previous-generation records (ADR 0047).
    /// Remove this decoder default one fleet roll after the field is written everywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleeping_until: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_store: Option<ResourceStoreDiagnostics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_sync: Option<flotilla_protocol::BlobSyncStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_generation: Option<String>,
    /// Wire-generation fingerprint compared by the daemon handshake.
    /// ADR 0047: previous-generation Host records omit this field; retain the
    /// decoder default until one fleet roll after all hosts publish it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_started_at: Option<DateTime<Utc>>,
    /// Available bytes on the host-direct checkout root used for convoy admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_free_bytes: Option<u64>,
    /// Daemon RSS from the latest heartbeat. Previous-generation Host records
    /// omit this field; keep the decoder default through the next fleet roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_rss_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_free_space_floor_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[builder(default)]
    pub conditions: Vec<HostCondition>,
    #[serde(default)]
    #[builder(default)]
    pub sleep_inhibition: SleepInhibitionHealth,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelProbeState {
    #[serde(default)]
    pub entries: BTreeMap<String, CachedModelProbe>,
    #[serde(default)]
    pub window_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub requests_in_window: u32,
    #[serde(default)]
    pub total_requests: u64,
}

impl ModelProbeState {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.window_started_at.is_none() && self.requests_in_window == 0 && self.total_requests == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedModelProbe {
    pub observed_at: DateTime<Utc>,
    /// None records an inconclusive request so it cannot be retried on every refresh.
    pub fact: Option<ModelFact>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FulfilmentFacts {
    #[serde(default)]
    pub harnesses: BTreeMap<String, HarnessFacts>,
    #[serde(default)]
    pub toolchains: BTreeMap<String, String>,
    pub gui_session_logged_in: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_vessel_slots: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<FulfilmentImage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_present: Option<bool>,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FulfilmentImage {
    pub image_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_image_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_digest: Option<String>,
}
impl From<String> for FulfilmentImage {
    fn from(image_ref: String) -> Self {
        Self { image_ref, local_image_id: None, registry_digest: None }
    }
}
impl<'de> Deserialize<'de> for FulfilmentImage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // ADR 0047: remove old string decoding one roll after generation 1.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Record {
            Legacy(String),
            Current { image_ref: String, local_image_id: Option<String>, registry_digest: Option<String> },
        }
        Ok(match Record::deserialize(deserializer)? {
            Record::Legacy(image_ref) => image_ref.into(),
            Record::Current { image_ref, local_image_id, registry_digest } => Self { image_ref, local_image_id, registry_digest },
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessFacts {
    pub version: String,
    #[serde(default)]
    pub models: BTreeMap<String, ModelFact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFact {
    pub usable: bool,
    pub source: ModelFactSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFactSource {
    Probe,
    Declaration,
}

impl HostStatus {
    /// Join descriptive observations with authoritative placement availability.
    pub fn host_summary(&self) -> Option<HostSummary> {
        let mut summary = self.description.clone()?;
        summary.providers.retain(|provider| {
            provider.category != AGENT_ADAPTER_PROVIDER_CATEGORY && provider.category != TERMINAL_POOL_PROVIDER_CATEGORY
        });
        summary.environments.retain(|environment| matches!(environment, EnvironmentInfo::Provisioned { .. }));
        for (key, category) in
            [(AGENT_ADAPTERS_CAPABILITY, AGENT_ADAPTER_PROVIDER_CATEGORY), (TERMINAL_POOLS_CAPABILITY, TERMINAL_POOL_PROVIDER_CATEGORY)]
        {
            if let Some(serde_json::Value::Array(implementations)) = self.capabilities.get(key) {
                for implementation in implementations.iter().filter_map(serde_json::Value::as_str) {
                    summary.providers.push(HostProviderStatus::available(category, implementation));
                }
            }
        }
        summary.providers.sort_by(|left, right| (&left.category, &left.name).cmp(&(&right.category, &right.name)));
        Some(summary)
    }

    pub fn is_agentless_ssh(&self) -> bool {
        self.capabilities.get(AGENTLESS_CAPABILITY) == Some(&serde_json::Value::Bool(true))
            && self.capabilities.get(TRANSPORT_CAPABILITY).and_then(serde_json::Value::as_str) == Some("ssh")
            && self.capabilities.get(PLACEMENT_CAPABILITY).and_then(serde_json::Value::as_str) == Some("host_direct_only")
    }

    pub fn agentless_ssh_owner(&self) -> Option<&str> {
        self.is_agentless_ssh().then(|| self.capabilities.get(OWNING_DAEMON_CAPABILITY)).flatten()?.as_str()
    }

    pub fn agent_adapters(&self) -> Result<BTreeSet<String>, serde_json::Error> {
        self.capabilities.get(AGENT_ADAPTERS_CAPABILITY).cloned().map(serde_json::from_value).transpose().map(Option::unwrap_or_default)
    }

    pub fn held_credentials(&self) -> Result<BTreeSet<String>, serde_json::Error> {
        self.capabilities.get(HELD_CREDENTIALS_CAPABILITY).cloned().map(serde_json::from_value).transpose().map(Option::unwrap_or_default)
    }

    /// Expiry metadata for held credential material, keyed by scope name —
    /// [`AMBIENT_CLAUDE_CREDENTIAL_SCOPE`] for the ambient claude login, a
    /// `CredentialSpec` name for declared credentials whose adapter can
    /// express expiry. Timestamps only; never material.
    pub fn credential_expiry(&self) -> Result<BTreeMap<String, CredentialExpiry>, serde_json::Error> {
        self.capabilities.get(CREDENTIAL_EXPIRY_CAPABILITY).cloned().map(serde_json::from_value).transpose().map(Option::unwrap_or_default)
    }

    /// Whether the last heartbeat is within the host readiness TTL.
    pub fn heartbeat_is_fresh(&self, now: DateTime<Utc>) -> bool {
        self.heartbeat_at
            .is_some_and(|heartbeat_at| now.signed_duration_since(heartbeat_at) <= chrono::Duration::seconds(HEARTBEAT_READY_TTL_SECS))
    }

    pub fn apply_heartbeat_readiness(&mut self, now: DateTime<Utc>) {
        self.ready = self.ready && !self.readiness_blocked() && self.heartbeat_is_fresh(now);
    }

    pub fn is_degraded(&self) -> bool {
        self.conditions.iter().any(|condition| condition.value == ConditionValue::False)
    }

    pub fn readiness_blocked(&self) -> bool {
        self.conditions.iter().any(HostCondition::blocks_readiness)
    }

    /// This status with every observation timestamp cleared, so two statuses
    /// compare equal when they observed the same things at different times.
    pub fn without_observation_times(&self) -> Self {
        let mut status = self.clone();
        status.heartbeat_at = None;
        for condition in &mut status.conditions {
            condition.observed_at = DateTime::UNIX_EPOCH;
        }
        for facts in status.fulfilment_facts.values_mut() {
            facts.observed_at = DateTime::UNIX_EPOCH;
        }
        status
    }
}

impl FulfilmentFacts {
    /// Whether two observations report the same facts, whenever they were made.
    pub fn same_observation(&self, other: &Self) -> bool {
        Self { observed_at: other.observed_at, ..self.clone() } == *other
    }

    /// Whether two fulfilment maps report the same facts per kind, whenever
    /// each was observed.
    pub fn same_observations(left: &BTreeMap<String, Self>, right: &BTreeMap<String, Self>) -> bool {
        left.len() == right.len()
            && left
                .iter()
                .zip(right)
                .all(|((left_kind, left), (right_kind, right))| left_kind == right_kind && left.same_observation(right))
    }
}

#[cfg(test)]
mod vessel_cap_tests {
    use super::{HostConnection, HostSpec};
    use crate::FulfilmentRealisation;

    // #3793, #4002: an agentless host's cap bounds every placement there; a
    // daemon host's cap bounds only its Docker per-vessel placements.
    #[test]
    fn vessel_cap_by_connection_and_realisation() {
        let agentless = HostConnection::AgentlessSsh { owning_daemon: "owner".into(), destination: "crew@b1".into() };
        let docker = FulfilmentRealisation::DockerPerVessel { image: None };
        let direct = FulfilmentRealisation::HostDirect;
        let cloud = FulfilmentRealisation::CloudSandboxPerVessel { image: "sandbox".into() };
        for (connection, realisation, capped) in [
            (agentless.clone(), &direct, true),
            (agentless, &docker, true),
            (HostConnection::Daemon, &docker, true),
            (HostConnection::Daemon, &direct, false),
            (HostConnection::Daemon, &cloud, false),
        ] {
            let host = HostSpec { connection: connection.clone(), max_vessels: Some(3), ..HostSpec::default() };
            assert_eq!(host.vessel_cap(realisation), capped.then_some(3), "{connection:?} {realisation:?}");
            assert_eq!(HostSpec { max_vessels: None, ..host }.vessel_cap(realisation), None);
        }
    }
}

#[cfg(test)]
mod build_parallelism_tests {
    use super::HostSpec;

    // Every crew can use the host's cores, independently of sizing expectations.
    #[hegel::test]
    fn host_build_jobs_use_all_cores(tc: hegel::TestCase) {
        // Covers zero/one-core boundaries, small hosts and fleet-sized hosts,
        // with zero, default and oversized crew counts.
        let cores = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(128));
        let crews = tc.draw(hegel::generators::integers::<u32>().min_value(0).max_value(64));
        let host = HostSpec { expected_concurrent_rust_crews: crews, ..Default::default() };
        assert_eq!(host.rust_build_jobs(cores), cores.max(1));
    }

    #[test]
    fn feta_and_small_hosts_use_all_cores() {
        let host = HostSpec::default();
        assert_eq!(host.rust_build_jobs(32), 32);
        assert_eq!(host.rust_build_jobs(4), 4);
        assert_eq!(host.rust_build_jobs(1), 1);
    }

    #[test]
    fn previous_generation_host_keeps_default_build_share() {
        let host: HostSpec = serde_json::from_str(r#"{"display_name":"feta"}"#).expect("decode previous host");
        assert_eq!(host.expected_concurrent_rust_crews, 4);
    }
}

/// Admission degradation that does not withdraw a live forge observer.
pub const PROJECTION_PARITY_CONDITION: &str = "ProjectionParity";
/// The local convoy projection has not caught up with durable rows.
pub const LOCAL_ROWS_MISSING_REASON: &str = "LocalRowsMissing";

/// A daemon-owned liveness invariant published on its [`Host`] resource.
///
/// `True` means the named subsystem is healthy. `False` surfaces a diagnostic;
/// by default it also makes the host unavailable for placement until a later
/// observation clears it, unless `blocks_readiness` marks it advisory.
///
/// # Example
///
/// New condition authors must decide whether to withdraw forge observation.
/// This example deliberately omits that decision and must not compile.
/// ```compile_fail
/// use flotilla_resources::{ConditionValue, HostCondition};
/// HostCondition::builder()
///     .condition_type("Example")
///     .value(ConditionValue::False)
///     .reason("Failed")
///     .message("example")
///     .observed_at(chrono::Utc::now())
///     .build();
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
#[serde(from = "StoredHostCondition")]
pub struct HostCondition {
    #[serde(rename = "type")]
    pub condition_type: String,
    pub value: ConditionValue,
    pub reason: String,
    pub message: String,
    pub observed_at: DateTime<Utc>,
    /// Whether this condition makes the host unavailable for placement.
    ///
    /// Conditions may also report diagnostic warnings about specific records
    /// or historical events without indicating current host unhealth.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    #[builder(default = true)]
    pub blocks_readiness: bool,
    /// Whether a false condition prevents this host from observing forges.
    /// Required by the builder independently of admission readiness.
    pub blocks_forge_observation: bool,
}

impl HostCondition {
    pub fn blocks_forge_observation(&self) -> bool {
        self.blocks_forge_observation && self.value == ConditionValue::False
    }

    pub fn blocks_readiness(&self) -> bool {
        self.blocks_readiness && self.value == ConditionValue::False
    }
}

// ADR 0047, N→N+1 only: previous Host conditions lack an observation
// decision. Preserve their old admission-blocking policy conservatively during
// decoding, until their owner republishes an explicit classification. Remove
// this record/from shim after one fleet roll; never default new builders.
#[derive(Deserialize)]
struct StoredHostCondition {
    #[serde(rename = "type")]
    condition_type: String,
    value: ConditionValue,
    reason: String,
    message: String,
    observed_at: DateTime<Utc>,
    #[serde(default = "default_true")]
    blocks_readiness: bool,
    #[serde(default)]
    blocks_forge_observation: Option<bool>,
}

impl From<StoredHostCondition> for HostCondition {
    fn from(stored: StoredHostCondition) -> Self {
        Self {
            condition_type: stored.condition_type,
            value: stored.value,
            reason: stored.reason,
            message: stored.message,
            observed_at: stored.observed_at,
            blocks_readiness: stored.blocks_readiness,
            blocks_forge_observation: stored.blocks_forge_observation.unwrap_or(stored.blocks_readiness),
        }
    }
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

/// Expiry metadata observed for one piece of held credential material.
///
/// Published under [`CREDENTIAL_EXPIRY_CAPABILITY`] in the same capability
/// family as `held_credentials`. Carries timestamps and nothing else — the
/// probe that produces it must never read tokens into these fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct CredentialExpiry {
    /// When the primary material (e.g. an access token) expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// When the refresh chain dies, where the material is refreshable. While
    /// this is in the future the primary material can be renewed, so it — not
    /// `expires_at` — bounds the credential's usable life.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_expires_at: Option<DateTime<Utc>>,
}

impl CredentialExpiry {
    /// The latest point the material can still authenticate: the refresh
    /// chain's expiry where one exists, otherwise the primary expiry. `None`
    /// means no expiry metadata was observed.
    pub fn effective_expires_at(&self) -> Option<DateTime<Utc>> {
        self.refresh_expires_at.max(self.expires_at)
    }

    /// The expiry timestamp, if the material is expired at `now`.
    pub fn expired_at(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.effective_expires_at().filter(|expires_at| *expires_at <= now)
    }

    /// The expiry timestamp, if the material is unexpired at `now` but
    /// expires within `window`.
    pub fn expires_within(&self, now: DateTime<Utc>, window: chrono::Duration) -> Option<DateTime<Utc>> {
        self.effective_expires_at().filter(|expires_at| *expires_at > now && *expires_at <= now + window)
    }
}

/// Latest periodic observations a heartbeat carries into its single write,
/// so their observers do not each commit the Host object.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FoldedHostObservations {
    /// Latest blob sync observation. `None` leaves the stored value unchanged.
    pub blob_sync: Option<flotilla_protocol::BlobSyncStatus>,
    /// Latest fulfilment observations. The heartbeat only advances the stored
    /// `observed_at` of facts whose content is unchanged; changed facts are
    /// published by the fulfilment writer itself.
    pub fulfilment_refresh: BTreeMap<String, FulfilmentFacts>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostStatusPatch {
    FulfilmentFacts {
        facts: BTreeMap<String, FulfilmentFacts>,
        model_probes: ModelProbeState,
    },
    Heartbeat {
        description: Option<Box<HostSummary>>,
        capabilities: BTreeMap<String, serde_json::Value>,
        heartbeat_at: DateTime<Utc>,
        ready: bool,
        daemon_generation: Option<String>,
        protocol_fingerprint: Option<String>,
        daemon_version: Option<String>,
        daemon_started_at: Option<DateTime<Utc>>,
        disk_free_bytes: Option<u64>,
        daemon_rss_bytes: Option<u64>,
        admission_free_space_floor_bytes: Option<u64>,
        agent_adapter_baseline: Option<BTreeSet<String>>,
        // Keep the in-process patch enum small; HostStatus stores the unboxed value.
        resource_store: Option<Box<ResourceStoreDiagnostics>>,
        conditions: Vec<HostCondition>,
        /// Periodic observations folded into the heartbeat's write (#3154).
        folded: Box<FoldedHostObservations>,
    },
    SleepInhibition {
        health: SleepInhibitionHealth,
        observed_at: DateTime<Utc>,
    },
}

impl StatusPatch<HostStatus> for HostStatusPatch {
    /// Host status is a latest-value observation. Commit only what changes an
    /// observation; fold timestamp-only refreshes into the heartbeat, which
    /// still commits often enough to keep `heartbeat_at` fresh.
    fn requires_write(&self, before: &HostStatus, after: &HostStatus) -> bool {
        if before.without_observation_times() != after.without_observation_times() {
            return true;
        }
        match self {
            Self::Heartbeat { heartbeat_at, .. } => before.heartbeat_at.is_none_or(|previous| {
                // A clock regression must not leave a future heartbeat looking fresh.
                *heartbeat_at < previous
                    || heartbeat_at.signed_duration_since(previous) >= chrono::Duration::seconds(HEARTBEAT_REFRESH_SECS)
            }),
            Self::FulfilmentFacts { .. } | Self::SleepInhibition { .. } => false,
        }
    }

    fn apply(&self, status: &mut HostStatus) {
        match self {
            Self::FulfilmentFacts { facts, model_probes } => {
                status.fulfilment_facts.clone_from(facts);
                status.model_probes.clone_from(model_probes);
            }
            Self::Heartbeat {
                description,
                capabilities,
                heartbeat_at,
                ready,
                daemon_generation,
                protocol_fingerprint,
                daemon_version,
                daemon_started_at,
                disk_free_bytes,
                daemon_rss_bytes,
                admission_free_space_floor_bytes,
                agent_adapter_baseline,
                resource_store,
                conditions,
                folded,
            } => {
                // Image inventory is observed independently of heartbeat gathering.
                // Preserve its latest value atomically when heartbeat has no update.
                let image_inventory = status.capabilities.get(crate::IMAGE_DIGESTS_CAPABILITY).cloned();
                status.capabilities = capabilities.clone();
                if let Some(inventory) = image_inventory {
                    status.capabilities.entry(crate::IMAGE_DIGESTS_CAPABILITY.into()).or_insert(inventory);
                }
                status.description = description.as_deref().cloned();
                // Placement capability keys are authoritative; do not persist a second
                // availability list in the descriptive provider health observations.
                if let Some(description) = &mut status.description {
                    description.providers.retain(|provider| {
                        provider.category != AGENT_ADAPTER_PROVIDER_CATEGORY && provider.category != TERMINAL_POOL_PROVIDER_CATEGORY
                    });
                }
                status.heartbeat_at = Some(*heartbeat_at);
                // Sleep inhibition has its own writer. Apply heartbeat conditions to
                // the latest status so a concurrent sleep update survives a retry.
                let sleep_conditions = status
                    .conditions
                    .iter()
                    .filter(|condition| condition.condition_type == SLEEP_INHIBITION_CONDITION_TYPE)
                    .cloned()
                    .collect::<Vec<_>>();
                status.conditions.clone_from(conditions);
                status.conditions.extend(sleep_conditions);
                status.ready = *ready && !status.readiness_blocked();
                status.daemon_generation.clone_from(daemon_generation);
                status.protocol_fingerprint.clone_from(protocol_fingerprint);
                status.daemon_version.clone_from(daemon_version);
                status.daemon_started_at = *daemon_started_at;
                status.disk_free_bytes = *disk_free_bytes;
                status.daemon_rss_bytes = *daemon_rss_bytes;
                status.admission_free_space_floor_bytes = *admission_free_space_floor_bytes;
                status.agent_adapter_baseline.clone_from(agent_adapter_baseline);
                status.resource_store = resource_store.as_deref().cloned();
                if let Some(blob_sync) = &folded.blob_sync {
                    status.blob_sync = Some(blob_sync.clone());
                }
                for (kind, observed) in &folded.fulfilment_refresh {
                    if let Some(stored) = status.fulfilment_facts.get_mut(kind) {
                        if stored.observed_at < observed.observed_at && stored.same_observation(observed) {
                            stored.observed_at = observed.observed_at;
                        }
                    }
                }
            }
            Self::SleepInhibition { health, observed_at } => {
                status.sleep_inhibition.clone_from(health);
                match health {
                    SleepInhibitionHealth::Failed { message, .. } => {
                        status.conditions.retain(|condition| condition.condition_type != SLEEP_INHIBITION_CONDITION_TYPE);
                        status.conditions.push(
                            HostCondition::builder()
                                .condition_type(SLEEP_INHIBITION_CONDITION_TYPE)
                                .value(ConditionValue::False)
                                .reason("InhibitorNotHeld")
                                .message(format!("sleep inhibition required but not held ({message})"))
                                .observed_at(*observed_at)
                                // An awake daemon can observe forges without sleep inhibition; sleep is detected by heartbeat expiry.
                                .blocks_forge_observation(false)
                                .build(),
                        );
                    }
                    SleepInhibitionHealth::Held | SleepInhibitionHealth::NotRequired => {
                        status.conditions.retain(|condition| condition.condition_type != SLEEP_INHIBITION_CONDITION_TYPE);
                    }
                    SleepInhibitionHealth::Acquiring { .. } => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Stored N records lack the new decision. Decode conservatively, while
    // every newly serialized record writes its explicit classification.
    #[test]
    fn previous_generation_condition_decodes_and_new_writes_are_explicit() {
        let old = serde_json::json!({
            "type": "CapabilityRegression", "value": "false",
            "reason": "AgentAdaptersMissing", "message": "missing adapter",
            "observed_at": "2026-10-17T00:00:00Z"
        });
        let legacy: HostCondition = serde_json::from_value(old.clone()).expect("previous condition");
        assert!(legacy.blocks_forge_observation());
        let mut advisory = old;
        advisory["blocks_readiness"] = serde_json::json!(false);
        assert!(!serde_json::from_value::<HostCondition>(advisory).expect("previous advisory").blocks_forge_observation());
        let current = HostCondition::builder()
            .condition_type("CapabilityRegression")
            .value(ConditionValue::False)
            .reason("AgentAdaptersMissing")
            .message("missing adapter")
            .observed_at(Utc::now())
            .blocks_forge_observation(false)
            .build();
        let encoded = serde_json::to_value(&current).expect("new condition");
        assert_eq!(encoded["blocks_forge_observation"], false);
        assert!(!serde_json::from_value::<HostCondition>(encoded).expect("current round trip").blocks_forge_observation());
    }

    #[test]
    fn failed_host_condition_makes_fresh_heartbeat_not_ready() {
        let now = Utc::now();
        let mut status = HostStatus {
            heartbeat_at: Some(now),
            ready: true,
            conditions: vec![HostCondition::builder()
                .condition_type("Controller/checkout")
                .value(ConditionValue::False)
                .reason("RestartBudgetExhausted")
                .message("checkout controller stopped")
                .observed_at(now)
                // This test admission condition does not disable forge reads.
                .blocks_forge_observation(false)
                .build()],
            ..HostStatus::default()
        };

        status.apply_heartbeat_readiness(now);

        assert!(!status.ready);
        let encoded = serde_json::to_value(&status).expect("serialize host status");
        assert_eq!(encoded["conditions"][0]["type"], "Controller/checkout");
    }

    #[test]
    fn advisory_host_condition_keeps_fresh_heartbeat_ready() {
        let now = Utc::now();
        let mut status = HostStatus {
            heartbeat_at: Some(now),
            ready: true,
            conditions: vec![HostCondition::builder()
                .condition_type("ResourceReplication/AuthorshipCollision")
                .value(ConditionValue::False)
                .reason("HomeBoundRecordAuthoredAtMultipleRoots")
                .message("record is authored at multiple roots")
                .observed_at(now)
                .blocks_readiness(false)
                // This test admission condition does not disable forge reads.
                .blocks_forge_observation(false)
                .build()],
            ..HostStatus::default()
        };

        status.apply_heartbeat_readiness(now);

        assert!(status.ready);
        assert!(status.is_degraded(), "advisory condition should remain visible as a diagnostic");
        assert!(!status.readiness_blocked());
    }

    #[test]
    fn credential_expiry_round_trips_through_the_capability_family() {
        let expires_at = Utc::now();
        let refresh_expires_at = expires_at + chrono::Duration::days(30);
        let expiry = CredentialExpiry::builder().expires_at(expires_at).refresh_expires_at(refresh_expires_at).build();
        let status = HostStatus {
            capabilities: BTreeMap::from([(
                CREDENTIAL_EXPIRY_CAPABILITY.to_string(),
                serde_json::json!({ AMBIENT_CLAUDE_CREDENTIAL_SCOPE: expiry }),
            )]),
            ..HostStatus::default()
        };

        let decoded = status.credential_expiry().expect("decode credential expiry");
        assert_eq!(decoded, BTreeMap::from([(AMBIENT_CLAUDE_CREDENTIAL_SCOPE.to_string(), expiry)]));
        assert_eq!(HostStatus::default().credential_expiry().expect("absent capability decodes"), BTreeMap::new());
    }

    #[test]
    fn credential_expiry_is_bounded_by_the_refresh_chain_where_one_exists() {
        let now = Utc::now();
        let window = chrono::Duration::days(7);
        let refreshable = CredentialExpiry::builder()
            .expires_at(now - chrono::Duration::hours(1))
            .refresh_expires_at(now + chrono::Duration::days(3))
            .build();
        assert_eq!(refreshable.effective_expires_at(), refreshable.refresh_expires_at);
        assert_eq!(refreshable.expired_at(now), None);
        assert_eq!(refreshable.expires_within(now, window), refreshable.refresh_expires_at);

        let dead = CredentialExpiry::builder()
            .expires_at(now - chrono::Duration::days(20))
            .refresh_expires_at(now - chrono::Duration::days(15))
            .build();
        assert_eq!(dead.expired_at(now), dead.refresh_expires_at);
        assert_eq!(dead.expires_within(now, window), None);

        let access_only = CredentialExpiry::builder().expires_at(now + chrono::Duration::days(30)).build();
        assert_eq!(access_only.effective_expires_at(), access_only.expires_at);
        assert_eq!(access_only.expired_at(now), None);
        assert_eq!(access_only.expires_within(now, window), None);

        assert_eq!(CredentialExpiry::default().effective_expires_at(), None);
        assert_eq!(CredentialExpiry::default().expired_at(now), None);
    }
}
