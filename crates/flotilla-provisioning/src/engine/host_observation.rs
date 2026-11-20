//! The observation a Host's status maps to (ADR 0047): the capability keys carried into a
//! HostObservation, and those dropped as read by nothing.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_protocol::provider::ProviderAdvertisement;
use flotilla_resources::{
    image_architecture::canonical_image_architecture, CredentialExpiry, FulfilmentFacts, HostConnection, HostSpec, HostStatus,
    LocalImageInventories, ModelProbeState, AGENT_ADAPTERS_CAPABILITY, CODEX_CENTRAL_LOGIN_CAPABILITY, CREDENTIAL_EXPIRY_CAPABILITY,
    ENVIRONMENT_PROVIDERS_CAPABILITY, HELD_CREDENTIALS_CAPABILITY, IMAGE_DIGESTS_CAPABILITY, OWNING_DAEMON_CAPABILITY,
    PLACEMENT_CAPABILITY, SSH_HOST_CAPABILITY, TERMINAL_POOLS_CAPABILITY, TRANSPORT_CAPABILITY,
};
use flotilla_schema::{Kind, Timestamp};

use crate::stored::host::{Condition, Expiry, Facts, HostObservation, Provider, ProviderImages};

const OBSERVATION: &str = <HostObservation as Kind>::KIND;

/// The capability keys whose values are not carried, with the reason counted.
const DROPPED_CAPABILITIES: [(&str, &str); 10] = [
    (TERMINAL_POOLS_CAPABILITY, "terminal pools, read only for display"),
    ("docker", "the docker and os capabilities, which no decision reads"),
    ("os", "the docker and os capabilities, which no decision reads"),
    ("io_pressure", "metrics"),
    ("store_read_retries", "metrics"),
    ("forge_budgets", "metrics"),
    (SSH_HOST_CAPABILITY, "the SSH capability markers, which the connection carries"),
    // The key the frozen fleet's SSH hosts still publish (pre-#4001); remove one roll after the
    // cut-over (#3108).
    ("agentless", "the SSH capability markers, which the connection carries"),
    (TRANSPORT_CAPABILITY, "the SSH capability markers, which the connection carries"),
    (PLACEMENT_CAPABILITY, "the SSH capability markers, which the connection carries"),
];

/// The capability keys carried into the observation.
const CARRIED_CAPABILITIES: [&str; 7] = [
    ENVIRONMENT_PROVIDERS_CAPABILITY,
    AGENT_ADAPTERS_CAPABILITY,
    HELD_CREDENTIALS_CAPABILITY,
    CREDENTIAL_EXPIRY_CAPABILITY,
    IMAGE_DIGESTS_CAPABILITY,
    CODEX_CENTRAL_LOGIN_CAPABILITY,
    OWNING_DAEMON_CAPABILITY,
];

fn capability<T: serde::de::DeserializeOwned + Default>(status: &HostStatus, key: &str) -> Result<T, String> {
    status
        .capabilities
        .get(key)
        .map(|value| serde_json::from_value(value.clone()).map_err(|error| format!("its `{key}` capability does not decode: {error}")))
        .transpose()
        .map(Option::unwrap_or_default)
}

/// The observation `status` maps to, with what it does not carry pushed to `dropped`.
pub fn observation(
    spec: &HostSpec,
    status: &HostStatus,
    dropped: &mut Vec<(&'static str, &'static str)>,
) -> Result<HostObservation, String> {
    let mut uncarried = |reason: &'static str| {
        if !dropped.contains(&(OBSERVATION, reason)) {
            dropped.push((OBSERVATION, reason));
        }
    };
    for key in status.capabilities.keys() {
        match DROPPED_CAPABILITIES.iter().find(|(capability, _)| capability == key) {
            Some((_, reason)) => uncarried(reason),
            None if CARRIED_CAPABILITIES.contains(&key.as_str()) => {}
            None => return Err(format!("its capability `{key}` has no import mapping")),
        }
    }
    let owner: Option<String> = capability(status, OWNING_DAEMON_CAPABILITY)?;
    if let (Some(owner), HostConnection::SshHost { owning_daemon, .. }) = (&owner, &spec.connection) {
        if owner != owning_daemon {
            return Err(format!("its `{OWNING_DAEMON_CAPABILITY}` capability names `{owner}`, and its connection `{owning_daemon}`"));
        }
    }
    let mut conditions = BTreeMap::new();
    for condition in &status.conditions {
        uncarried("each condition's observation time, a judgement");
        // A remote's reachability blocks neither, but the clones that reach it wait on it.
        let remote = condition.condition_type.starts_with(flotilla_resources::REMOTE_CONDITION_PREFIX);
        if !condition.blocks_readiness && !condition.blocks_forge_observation && !remote {
            uncarried("diagnostic conditions, which belong in metrics");
            continue;
        }
        if conditions.insert(condition.condition_type.clone(), Condition::new(condition)).is_some() {
            return Err(format!("it holds condition `{}` twice", condition.condition_type));
        }
    }
    let mut providers = BTreeMap::new();
    for advertisement in capability::<Vec<ProviderAdvertisement>>(status, ENVIRONMENT_PROVIDERS_CAPABILITY)? {
        if advertisement.kind.is_some() {
            uncarried("provider implementation names, which nothing reads");
        }
        let provider = Provider::new(&advertisement).map_err(|reason| format!("provider `{}`: {reason}", advertisement.instance))?;
        if providers.insert(advertisement.instance.clone(), provider).is_some() {
            return Err(format!("it advertises provider instance `{}` twice", advertisement.instance));
        }
    }
    let agent_adapters: BTreeSet<String> = capability(status, AGENT_ADAPTERS_CAPABILITY)?;
    let credential_expiry: BTreeMap<String, CredentialExpiry> = capability(status, CREDENTIAL_EXPIRY_CAPABILITY)?;
    let images: LocalImageInventories = capability(status, IMAGE_DIGESTS_CAPABILITY)?;
    let codex_central_login: Option<bool> = capability(status, CODEX_CENTRAL_LOGIN_CAPABILITY)?;
    let description = status.description.as_ref();
    if description.is_some() {
        uncarried("the host summary but its architecture, display only");
    }
    if status.daemon_version.is_some() || status.daemon_started_at.is_some() {
        uncarried("the daemon's version and start time, display only");
    }
    if status.sleep_inhibition != Default::default() {
        uncarried("the sleep inhibition state, which its condition carries");
    }
    if status.heartbeat_at.is_some() || status.ready {
        uncarried("heartbeat time and readiness, a judgement");
    }
    if status.sleeping_until.is_some() {
        uncarried("sleeping_until, which nothing sets");
    }
    if status.model_probes != ModelProbeState::default() {
        uncarried("model probes, which nothing keeps");
    }
    if status.disk_free_bytes.is_some() || status.admission_free_space_floor_bytes.is_some() {
        uncarried("the disk figures, carried only as the free-space floor flag");
    }
    if status.resource_store.is_some() || status.blob_sync.is_some() {
        uncarried("metrics");
    }
    // Without a vessel cap only a missing pool gives no free slots; under a cap a full host does too,
    // so there the first probe says whether the pool is missing.
    let pool_missing = |facts: &FulfilmentFacts| spec.max_vessels.is_none() && facts.free_vessel_slots == Some(0);
    for facts in status.fulfilment_facts.values() {
        uncarried("each fact's observation time, a judgement");
        if facts.image.is_some() || facts.image_present.is_some() {
            uncarried("image identity and presence, which nothing reads");
        }
        if facts.free_vessel_slots.is_some() && !pool_missing(facts) {
            uncarried("free vessel slots, a judgement");
        }
        if facts.harnesses.values().any(|harness| !harness.models.is_empty()) {
            uncarried("model facts, which nothing keeps");
        }
    }
    Ok(HostObservation {
        daemon_generation: status.daemon_generation.clone(),
        protocol_fingerprint: status.protocol_fingerprint.clone(),
        architecture: description
            .and_then(|summary| summary.system.arch.as_deref())
            .map(|arch| canonical_image_architecture(arch).to_owned()),
        below_free_space_floor: match (status.disk_free_bytes, status.admission_free_space_floor_bytes) {
            (Some(free), Some(floor)) => Some(free < floor),
            // No figures at all: a status read back from the engine and written again carries
            // the judgement it read, which stands.
            (None, None) => status.below_free_space_floor,
            // Either figure unknown: unknown.
            _ => None,
        },
        codex_central_login: codex_central_login.unwrap_or_default(),
        conditions,
        providers,
        // A previous generation without a baseline compared against its adapters.
        agent_adapter_baseline: status.agent_adapter_baseline.clone().unwrap_or_else(|| agent_adapters.clone()),
        agent_adapters,
        held_credentials: capability(status, HELD_CREDENTIALS_CAPABILITY)?,
        credential_expiry: credential_expiry
            .into_iter()
            .map(|(scope, expiry)| {
                let expiry = Expiry {
                    expires_at: expiry.expires_at.as_ref().map(Timestamp::from_datetime),
                    refresh_expires_at: expiry.refresh_expires_at.as_ref().map(Timestamp::from_datetime),
                };
                (scope, expiry)
            })
            .collect(),
        image_digests: images.0.into_iter().map(|(instance, images)| (instance, ProviderImages { images })).collect(),
        facts: status.fulfilment_facts.iter().map(|(kind, facts)| (kind.clone(), Facts::new(facts, pool_missing(facts)))).collect(),
    })
}
