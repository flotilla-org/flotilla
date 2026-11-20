//! The observation a Host's status maps to (ADR 0047): the capability keys carried into a
//! HostObservation, and those ignored as read by nothing.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_protocol::provider::ProviderAdvertisement;
use flotilla_resources::{
    image_architecture::canonical_image_architecture, CredentialExpiry, FulfilmentFacts, HostConnection, HostSpec, HostStatus,
    LocalImageInventories, AGENT_ADAPTERS_CAPABILITY, CODEX_CENTRAL_LOGIN_CAPABILITY, CREDENTIAL_EXPIRY_CAPABILITY,
    ENVIRONMENT_PROVIDERS_CAPABILITY, HELD_CREDENTIALS_CAPABILITY, IMAGE_DIGESTS_CAPABILITY, OWNING_DAEMON_CAPABILITY,
    PLACEMENT_CAPABILITY, SSH_HOST_CAPABILITY, TERMINAL_POOLS_CAPABILITY, TRANSPORT_CAPABILITY,
};
use flotilla_schema::Timestamp;

use crate::stored::host::{Condition, Expiry, Facts, HostObservation, Provider, ProviderImages};

/// The capability keys whose values are not carried: read by nothing, metrics, or carried by the
/// connection.
const IGNORED_CAPABILITIES: [&str; 10] = [
    TERMINAL_POOLS_CAPABILITY,
    "docker",
    "os",
    "io_pressure",
    "store_read_retries",
    "forge_budgets",
    SSH_HOST_CAPABILITY,
    // The key the frozen fleet's SSH hosts still publish (pre-#4001); remove one roll after the
    // cut-over (#3108).
    "agentless",
    TRANSPORT_CAPABILITY,
    PLACEMENT_CAPABILITY,
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

/// The observation `status` maps to.
pub fn observation(spec: &HostSpec, status: &HostStatus) -> Result<HostObservation, String> {
    for key in status.capabilities.keys() {
        if !IGNORED_CAPABILITIES.contains(&key.as_str()) && !CARRIED_CAPABILITIES.contains(&key.as_str()) {
            return Err(format!("its capability `{key}` is neither carried nor ignored"));
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
        // A remote's reachability blocks neither, but the clones that reach it wait on it.
        let remote = condition.condition_type.starts_with(flotilla_resources::REMOTE_CONDITION_PREFIX);
        if !condition.blocks_readiness && !condition.blocks_forge_observation && !remote {
            continue;
        }
        if conditions.insert(condition.condition_type.clone(), Condition::new(condition)).is_some() {
            return Err(format!("it holds condition `{}` twice", condition.condition_type));
        }
    }
    let mut providers = BTreeMap::new();
    for advertisement in capability::<Vec<ProviderAdvertisement>>(status, ENVIRONMENT_PROVIDERS_CAPABILITY)? {
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
    // Without a vessel cap only a missing pool gives no free slots; under a cap a full host does too,
    // so there the first probe says whether the pool is missing.
    let pool_missing = |facts: &FulfilmentFacts| spec.max_vessels.is_none() && facts.free_vessel_slots == Some(0);
    for _facts in status.fulfilment_facts.values() {}
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
