//! Host's import onto the engine (#3824 layer 4c, ADR 0047).
//!
//! A daemon's Host is homed at that daemon, and an SSH host's at its owning daemon, its only
//! writer. Each copy a home holds of a Host it neither describes nor owns is a stray, dropped.
//!
//! | Old copy | Imported |
//! |---|---|
//! | Deleted | dropped |
//! | `spec` | a Host record; `expected_concurrent_rust_crews` dropped, as nothing reads it |
//! | `status` | a HostObservation record: generation, protocol fingerprint, architecture, the free-space floor as a flag, conditions (`observed_at` as `since`), providers, adapters and their baseline, held credentials and their expiry, image digests, the Codex central login |
//! | `status.fulfilment_facts` | facts by FulfilmentKind name: GUI login, image and its presence, harness and toolchain versions; each fact's observation time, free vessel slots and model facts dropped, as judgements or kept by nothing |
//! | Heartbeat time, readiness, `sleeping_until`, model probes | dropped: readiness is a judgement, and nothing sets the others |
//! | The host summary (but its architecture), daemon version and start time, sleep inhibition state | dropped: display only; the sleep condition carries what readiness needs |
//! | Free disk and the free-space floor | only whether free disk is below the floor |
//! | Memory, store and blob-sync figures, `io_pressure`, `store_read_retries`, `forge_budgets` | dropped: metrics |
//! | `terminal_pools`, `docker` and `os` capabilities | dropped: no decision reads them |
//! | The SSH capability markers | dropped: the connection carries them |
//!
//! Refused: a copy at a home whose own host is not known; a capability key not named above; a
//! capability value that does not decode; a provider instance or condition type held twice; an
//! SSH host whose `owning_daemon` capability names an owner other than its connection's; a name
//! one home holds in two namespaces, since the engine has none.

use std::collections::{BTreeMap, BTreeSet};

use flotilla_engine_store::ImportTarget;
use flotilla_protocol::provider::ProviderAdvertisement;
use flotilla_resources::{
    image_architecture::canonical_image_architecture, CredentialExpiry, Host, HostConnection, HostStatus, LocalImageInventories,
    ModelProbeState, ResourceObject, AGENT_ADAPTERS_CAPABILITY, CODEX_CENTRAL_LOGIN_CAPABILITY, CREDENTIAL_EXPIRY_CAPABILITY,
    ENVIRONMENT_PROVIDERS_CAPABILITY, HELD_CREDENTIALS_CAPABILITY, IMAGE_DIGESTS_CAPABILITY, OWNING_DAEMON_CAPABILITY,
    PLACEMENT_CAPABILITY, SSH_HOST_CAPABILITY, TERMINAL_POOLS_CAPABILITY, TRANSPORT_CAPABILITY,
};
use flotilla_schema::{Kind, Timestamp};

use super::import::{created, ImportRefusal, ImportReport};
use crate::stored::host::{self as stored, Condition, Expiry, Facts, HostObservation, HostRecord, Provider, ProviderImages};

const HOST: &str = <HostRecord as Kind>::KIND;
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
    // The key SSH hosts published before #4001.
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

/// One old copy of a Host: its namespace, the object, and the home that wrote it.
pub struct HostCopy {
    pub namespace: String,
    pub object: ResourceObject<Host>,
    pub home: String,
}

#[derive(Default)]
pub struct HostCopies {
    pub copies: Vec<HostCopy>,
    /// Each home's own host: the Host its daemon describes.
    pub homes: BTreeMap<String, String>,
}

/// What an old copy imports to.
#[derive(Debug)]
pub enum Imported {
    Records {
        host: Box<HostRecord>,
        /// None for a Host with no status yet.
        observation: Option<Box<HostObservation>>,
        /// What was not carried, by kind and reason.
        dropped: Vec<(&'static str, &'static str)>,
    },
    /// Not carried, for the reason named.
    Dropped(&'static str),
}

/// The records `old`, held at a home whose own host is `own_host`, imports to, or why it is
/// dropped; refused with the reason.
pub fn import_host(old: &ResourceObject<Host>, own_host: &str) -> Result<Imported, String> {
    if old.metadata.deletion_timestamp.is_some() {
        return Ok(Imported::Dropped("deleted"));
    }
    match &old.spec.connection {
        HostConnection::Daemon if old.metadata.name != own_host => return Ok(Imported::Dropped("a copy of another daemon's Host")),
        HostConnection::SshHost { owning_daemon, .. } if owning_daemon != own_host => {
            return Ok(Imported::Dropped("a copy of an SSH host another daemon owns"))
        }
        _ => {}
    }
    let mut dropped = vec![(HOST, "expected_concurrent_rust_crews, which nothing reads")];
    let observation = old.status.as_ref().map(|status| observation(&old.spec.connection, status, &mut dropped)).transpose()?;
    Ok(Imported::Records { host: Box::new(HostRecord::new(&old.spec)), observation: observation.map(Box::new), dropped })
}

fn capability<T: serde::de::DeserializeOwned + Default>(status: &HostStatus, key: &str) -> Result<T, String> {
    status
        .capabilities
        .get(key)
        .map(|value| serde_json::from_value(value.clone()).map_err(|error| format!("its `{key}` capability does not decode: {error}")))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn observation(
    connection: &HostConnection,
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
    if let (Some(owner), HostConnection::SshHost { owning_daemon, .. }) = (&owner, connection) {
        if owner != owning_daemon {
            return Err(format!("its `{OWNING_DAEMON_CAPABILITY}` capability names `{owner}`, and its connection `{owning_daemon}`"));
        }
    }
    let mut conditions = BTreeMap::new();
    for condition in &status.conditions {
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
    if status.daemon_rss_bytes.is_some() || status.resource_store.is_some() || status.blob_sync.is_some() {
        uncarried("metrics");
    }
    for facts in status.fulfilment_facts.values() {
        uncarried("each fact's observation time, a judgement");
        if facts.free_vessel_slots.is_some() {
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
        below_free_space_floor: matches!((status.disk_free_bytes, status.admission_free_space_floor_bytes), (Some(free), Some(floor)) if free < floor),
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
        facts: status.fulfilment_facts.iter().map(|(kind, facts)| (kind.clone(), Facts::new(facts))).collect(),
    })
}

/// [`import_hosts`] into a temporary store of their own.
pub fn rehearse_hosts(copies: &HostCopies, report: &mut ImportReport) {
    match stored::registry().map_err(|error| format!("registry: {error:?}")).and_then(flotilla_engine_store::Rehearsal::open) {
        Ok(mut rehearsal) => import_hosts(copies, &mut rehearsal, report),
        Err(reason) => report.refused.push(ImportRefusal { kind: HOST, record: "<all>".to_owned(), reason }),
    }
}

/// Maps every copy at a home `target` imports onto the engine and writes its Host and
/// HostObservation, each at the copy's creation time; drops are counted and refusals named in
/// `report`.
pub fn import_hosts(copies: &HostCopies, target: &mut impl ImportTarget, report: &mut ImportReport) {
    let own: Vec<_> = copies.copies.iter().filter(|copy| target.imports(&copy.home)).collect();
    let mut namespaces = BTreeMap::<(&str, &str), BTreeSet<&str>>::new();
    for copy in own.iter().filter(|copy| copy.object.metadata.deletion_timestamp.is_none()) {
        namespaces.entry((copy.object.metadata.name.as_str(), copy.home.as_str())).or_default().insert(copy.namespace.as_str());
    }
    for copy in own {
        let name = &copy.object.metadata.name;
        let refuse = |reason: String| ImportRefusal { kind: HOST, record: name.clone(), reason: format!("at `{}`: {reason}", copy.home) };
        let held = namespaces.get(&(name.as_str(), copy.home.as_str())).cloned().unwrap_or_default();
        if held.len() > 1 {
            let held: Vec<_> = held.into_iter().collect();
            report.refused.push(refuse(format!("held in namespaces `{}`, and the engine has no namespaces", held.join("`, `"))));
            continue;
        }
        let Some(own_host) = copies.homes.get(&copy.home) else {
            report.refused.push(refuse("this home's own host is not known, so neither is whether the Host is homed here".to_owned()));
            continue;
        };
        match import_host(&copy.object, own_host) {
            Ok(Imported::Records { host, observation, dropped }) => {
                // Each record starts at its home as a fresh write; its old versions are not carried.
                // A copy counts only once both are written: a refusal fails the whole import, so a
                // Host written without its observation is never put in place.
                let change = created(&copy.object);
                let written = target.write(&copy.home, name, &change, &*host).map_err(refuse).and_then(|()| match &observation {
                    Some(observation) => target
                        .write(&copy.home, name, &change, &**observation)
                        .map_err(|reason| ImportRefusal { kind: OBSERVATION, ..refuse(reason) }),
                    None => Ok(()),
                });
                if let Err(refusal) = written {
                    report.refused.push(refusal);
                    continue;
                }
                *report.imported.entry((HOST, "spec")).or_default() += 1;
                if observation.is_some() {
                    *report.imported.entry((OBSERVATION, "observed")).or_default() += 1;
                }
                for key in dropped {
                    *report.dropped.entry(key).or_default() += 1;
                }
            }
            Ok(Imported::Dropped(reason)) => *report.dropped.entry((HOST, reason)).or_default() += 1,
            Err(reason) => report.refused.push(refuse(reason)),
        }
    }
}
