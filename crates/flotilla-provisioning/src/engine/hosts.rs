//! Host on the engine, behind `[primitives]`.
//!
//! [`EngineHosts`] is the [`EngineRecords`] port of Host over the decided store. A Host is two plain
//! records homed at the daemon that writes them: `Host`, the spec, and `HostObservation`, what that
//! daemon last observed there. This home writes its own host's and each SSH host's it owns; every
//! home's are read from a resident map kept current by following the log.
//!
//! The port is the observation's single writer at its home. A status written through it is mapped
//! onto the observation and compared with the one held: a value that changed is written, and an
//! unchanged one writes nothing. A condition keeps its reason and message while its value and
//! flags stand. Nothing is written on a timer, and no heartbeat time is stored.
//!
//! A Host reads back as the old object, with the judgements its readers expect: `heartbeat_at` is
//! when its home was last heard (now, for this home), `ready` is a fresh home with no condition
//! blocking readiness, and each observation time is that heartbeat time.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use chrono::{DateTime, Utc};
use flotilla_engine_store::{
    follow::{Followed, Follower},
    plain::{PendingChange, PlainError, PlainLog},
};
use flotilla_protocol::{
    provider::{ProviderAdvertisement, ProviderCapabilities, ProviderKind, ProviderReadiness},
    qualified_path::HostId,
    EnvironmentId, HostSummary, NodeId, NodeInfo, SystemInfo,
};
use flotilla_resources::{
    engine_records::{EngineObject, EngineRecords},
    CredentialExpiry, FulfilmentFacts, HarnessFacts, HostCondition, HostConnection, HostSpec, HostStatus, LocalImageInventories,
    AGENT_ADAPTERS_CAPABILITY, CODEX_CENTRAL_LOGIN_CAPABILITY, CREDENTIAL_EXPIRY_CAPABILITY, ENVIRONMENT_PROVIDERS_CAPABILITY,
    HELD_CREDENTIALS_CAPABILITY, IMAGE_DIGESTS_CAPABILITY, OWNING_DAEMON_CAPABILITY, PLACEMENT_CAPABILITY, SSH_HOST_CAPABILITY,
    TRANSPORT_CAPABILITY,
};
use flotilla_schema::{Kind, KindCode, NewChange, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, StoreError};
use futures::stream::BoxStream;
use serde_json::{json, Value};

use super::host_import;
use crate::stored::host::{Class, Condition, HostObservation, HostRecord, Readiness};

const KIND: &str = <HostRecord as Kind>::KIND;
const HOST: KindCode = <HostRecord as Kind>::CODE;
const OBSERVATION: KindCode = <HostObservation as Kind>::CODE;

/// How often a change stream looks for new entries in the log, as the other ports' do.
const POLL: Duration = Duration::from_millis(500);

/// How often a write that loses a race to another writer of its name reads and tries again.
const WRITE_ATTEMPTS: usize = 3;

/// The writer of a Host's spec: its seeding, an SSH host's registration, or an operator's apply.
const SPEC_WRITER: &str = "host-spec";
/// The writer of a Host's observation: every observer at its home, through this port.
const OBSERVER: &str = "host-observer";

/// One home's records of one Host, as the resident map holds them.
#[derive(Clone, Debug)]
struct Held {
    host: Option<(HostRecord, u64)>,
    observation: Option<(HostObservation, u64)>,
}

/// Every home's records, by name and home.
type Records = BTreeMap<(String, String), Held>;

struct Resident {
    follower: Follower,
    records: Records,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn reload(read: &mut Read<'_>, records: &mut Records, name: &str, home: &str) -> Result<(), StoreError> {
    let host = match read.resolve(HOST, name, home)? {
        Some(id) => read.load::<HostRecord>(id)?.map(|loaded| (loaded.value, loaded.version)),
        None => None,
    };
    let observation = match read.resolve(OBSERVATION, name, home)? {
        Some(id) => read.load::<HostObservation>(id)?.map(|loaded| (loaded.value, loaded.version)),
        None => None,
    };
    let key = (name.to_owned(), home.to_owned());
    if host.is_none() && observation.is_none() {
        records.remove(&key);
    } else {
        records.insert(key, Held { host, observation });
    }
    Ok(())
}

fn rescan(read: &mut Read<'_>, records: &mut Records, home: &str) -> Result<(), StoreError> {
    records.retain(|(_, owner), _| owner != home);
    let mut names = Vec::new();
    for code in [HOST, OBSERVATION] {
        let mut after = None;
        loop {
            let page = match code {
                HOST => read.scan::<HostRecord>(home, after, 64)?,
                _ => read.scan::<HostObservation>(home, after, 64)?,
            };
            for id in page.ids {
                let name = match code {
                    HOST => read.load::<HostRecord>(id)?.map(|loaded| loaded.name),
                    _ => read.load::<HostObservation>(id)?.map(|loaded| loaded.name),
                };
                names.extend(name);
            }
            after = page.next;
            if after.is_none() {
                break;
            }
        }
    }
    for name in names {
        reload(read, records, &name, home)?;
    }
    Ok(())
}

/// `name`'s records as several homes' resolve at `home`: its own, else the lexically first home's.
fn resolve<'a>(records: &'a Records, home: &str, name: &str) -> Option<(&'a str, &'a Held)> {
    let mut held = records.range((name.to_owned(), String::new())..).take_while(|((held, _), _)| held == name);
    let first = held.next()?;
    let ((_, owner), chosen) = std::iter::once(first).chain(held).find(|((_, owner), _)| owner == home).unwrap_or(first);
    Some((owner.as_str(), chosen))
}

/// The observation `status` maps to, with each condition whose value and flags stand kept as
/// `held` has it.
pub fn observed(spec: &HostSpec, status: &HostStatus, held: Option<&HostObservation>) -> Result<HostObservation, String> {
    let mut observation = host_import::observation(spec, status, &mut Vec::new())?;
    if let Some(held) = held {
        for (condition_type, condition) in &mut observation.conditions {
            if let Some(kept) = held.conditions.get(condition_type).filter(|kept| stands(kept, condition)) {
                condition.clone_from(kept);
            }
        }
    }
    Ok(observation)
}

/// Whether `observed` leaves `held`'s value and flags as they were.
fn stands(held: &Condition, observed: &Condition) -> bool {
    (held.value, held.blocks_readiness, held.blocks_forge_observation)
        == (observed.value, observed.blocks_readiness, observed.blocks_forge_observation)
}

/// The status `observation` of Host `name` reads back as, held at `home` and last heard from at
/// `heard`, judged at `now`.
pub fn status(
    name: &str,
    home: &str,
    spec: &HostSpec,
    observation: &HostObservation,
    heard: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> HostStatus {
    let observed_at = heard.unwrap_or(DateTime::UNIX_EPOCH);
    let conditions: Vec<HostCondition> = observation
        .conditions
        .iter()
        .map(|(condition_type, condition)| HostCondition {
            condition_type: condition_type.clone(),
            value: condition.value.into(),
            reason: condition.reason.clone(),
            message: condition.message.clone(),
            observed_at,
            blocks_readiness: condition.blocks_readiness,
            blocks_forge_observation: condition.blocks_forge_observation,
        })
        .collect();
    // The summary carries only the architecture, which image builds and their placement read.
    let description = observation.architecture.as_ref().map(|arch| HostSummary {
        environment_id: EnvironmentId::host(HostId::new(name)),
        host_name: None,
        node: NodeInfo::new(NodeId::new(home), spec.display_name.clone()),
        system: SystemInfo { arch: Some(arch.clone()), ..SystemInfo::default() },
        inventory: Default::default(),
        providers: Vec::new(),
        environments: Vec::new(),
    });
    // The figures stand in for the stored flag: below the floor reads as nothing free of one byte,
    // else as no floor.
    let (disk_free_bytes, admission_free_space_floor_bytes) =
        if observation.below_free_space_floor { (Some(0), Some(1)) } else { (None, Some(0)) };
    let mut status = HostStatus {
        description,
        capabilities: capabilities(&spec.connection, observation),
        fulfilment_facts: observation
            .facts
            .iter()
            .map(|(kind, facts)| {
                let facts = FulfilmentFacts {
                    harnesses: facts
                        .harnesses
                        .iter()
                        .map(|(harness, version)| (harness.clone(), HarnessFacts { version: version.clone(), models: BTreeMap::new() }))
                        .collect(),
                    toolchains: facts.toolchains.clone(),
                    gui_session_logged_in: facts.gui_session_logged_in,
                    free_vessel_slots: facts.crew_pool_missing.then_some(0),
                    image: None,
                    image_present: None,
                    observed_at,
                };
                (kind.clone(), facts)
            })
            .collect(),
        agent_adapter_baseline: Some(observation.agent_adapter_baseline.clone()),
        heartbeat_at: heard,
        ready: true,
        daemon_generation: observation.daemon_generation.clone(),
        disk_free_bytes,
        admission_free_space_floor_bytes,
        conditions,
        ..HostStatus::default()
    };
    status.apply_heartbeat_readiness(now);
    status
}

/// The capability map the observation's child tables, and `connection`'s SSH markers, rebuild.
fn capabilities(connection: &HostConnection, observation: &HostObservation) -> BTreeMap<String, Value> {
    let providers: Vec<ProviderAdvertisement> = observation
        .providers
        .iter()
        .map(|(instance, provider)| ProviderAdvertisement {
            instance: instance.clone(),
            kind: provider.class.and_then(Class::environment_class).map(|class| ProviderKind::new(class, String::new())),
            capabilities: ProviderCapabilities {
                material_modes: provider.material_modes.clone(),
                checkout_strategies: provider.checkout_strategies.clone(),
                workspace_root: provider.workspace_root.clone().into(),
                ..ProviderCapabilities::default()
            },
            readiness: match &provider.readiness {
                Readiness::Ready => ProviderReadiness::Ready,
                Readiness::MissingTool { reason } => ProviderReadiness::MissingTool { reason: reason.clone() },
                Readiness::MissingCredential { reason } => ProviderReadiness::MissingCredential { reason: reason.clone() },
                Readiness::Unknown { reason } => ProviderReadiness::Unknown { reason: reason.clone() },
            },
        })
        .collect();
    let expiry: BTreeMap<String, CredentialExpiry> = observation
        .credential_expiry
        .iter()
        .map(|(scope, expiry)| {
            let expiry = CredentialExpiry {
                expires_at: expiry.expires_at.and_then(Timestamp::to_datetime),
                refresh_expires_at: expiry.refresh_expires_at.and_then(Timestamp::to_datetime),
            };
            (scope.clone(), expiry)
        })
        .collect();
    let images = LocalImageInventories(
        observation.image_digests.iter().map(|(instance, images)| (instance.clone(), images.images.clone())).collect(),
    );
    let mut capabilities = BTreeMap::from([
        (ENVIRONMENT_PROVIDERS_CAPABILITY.to_owned(), json!(providers)),
        (AGENT_ADAPTERS_CAPABILITY.to_owned(), json!(observation.agent_adapters)),
        (HELD_CREDENTIALS_CAPABILITY.to_owned(), json!(observation.held_credentials)),
        (CREDENTIAL_EXPIRY_CAPABILITY.to_owned(), json!(expiry)),
        (IMAGE_DIGESTS_CAPABILITY.to_owned(), json!(images)),
    ]);
    if observation.codex_central_login {
        capabilities.insert(CODEX_CENTRAL_LOGIN_CAPABILITY.to_owned(), json!(true));
    }
    if let HostConnection::SshHost { owning_daemon, .. } = connection {
        capabilities.extend([
            (SSH_HOST_CAPABILITY.to_owned(), json!(true)),
            (TRANSPORT_CAPABILITY.to_owned(), json!("ssh")),
            (PLACEMENT_CAPABILITY.to_owned(), json!("host_direct_only")),
            (OWNING_DAEMON_CAPABILITY.to_owned(), json!(owning_daemon)),
        ]);
    }
    capabilities
}

/// The Hosts of the decided store at one home.
pub struct EngineHosts {
    service: Arc<Service>,
    home: String,
    /// The host this home's daemon describes: the Hosts homed here are its own and its SSH hosts'.
    own_host: String,
    log: PlainLog,
    resident: Mutex<Resident>,
    /// Held across a write's read and its edit, so the observers at this home write one at a time.
    writing: tokio::sync::Mutex<()>,
    clock: Arc<dyn flotilla_resources::Clock>,
}

impl std::fmt::Debug for EngineHosts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("EngineHosts").field("home", &self.home).field("own_host", &self.own_host).finish_non_exhaustive()
    }
}

impl EngineHosts {
    /// Opens this home's Host writer over `service`, for the daemon describing `own_host`, and reads
    /// every home's records.
    pub fn open(service: Arc<Service>, own_host: String, clock: Arc<dyn flotilla_resources::Clock>) -> Result<Self, String> {
        let log = PlainLog::new(service.store());
        let home = log.home().to_owned();
        let follower = Follower::new([HOST, OBSERVATION]);
        let opened = Self {
            service,
            home,
            own_host,
            log,
            resident: Mutex::new(Resident { follower, records: Records::new() }),
            writing: tokio::sync::Mutex::new(()),
            clock,
        };
        let homes = opened.service.homes();
        {
            let mut resident = locked(&opened.resident);
            let Resident { follower, records } = &mut *resident;
            opened
                .service
                .with_store(|store| {
                    let mut read = store.read()?;
                    follower.start(&mut read, &homes)?;
                    for home in &homes {
                        rescan(&mut read, records, home)?;
                    }
                    Ok(())
                })
                .map_err(|error| error.to_string())?;
        }
        Ok(opened)
    }

    /// Brings the resident map up to the store: every entry written since the last refresh.
    fn refresh(&self) -> Result<MutexGuard<'_, Resident>, String> {
        let mut resident = locked(&self.resident);
        let homes = self.service.homes();
        {
            let Resident { follower, records } = &mut *resident;
            self.service
                .with_store(|store| {
                    let mut read = store.read()?;
                    for followed in follower.pass(&mut read, &homes)? {
                        match followed {
                            Followed::Changed(identity) => reload(&mut read, records, &identity.name, &identity.home)?,
                            Followed::Rescan(home) => rescan(&mut read, records, &home)?,
                        }
                    }
                    Ok(())
                })
                .map_err(|error| error.to_string())?;
        }
        Ok(resident)
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    /// When `home` was last heard: now for this home, else its change stream's watermark.
    fn heard(&self, home: &str) -> Option<DateTime<Utc>> {
        if home == self.home {
            return Some(self.now());
        }
        self.service
            .watermark(home)
            .ok()
            .filter(|watermark| watermark.applied_through != Timestamp::ZERO)
            .and_then(|watermark| watermark.applied_through.to_datetime())
    }

    /// `held`, at `home`, as the old object; None for an observation whose Host is gone.
    fn object(&self, name: &str, home: &str, held: &Held) -> Option<Result<EngineObject, String>> {
        let (host, host_version) = held.host.as_ref()?;
        let spec = host.spec();
        let status = held.observation.as_ref().map(|(observation, _)| status(name, home, &spec, observation, self.heard(home), self.now()));
        let encoded = serde_json::to_value(&spec)
            .and_then(|spec| Ok((spec, status.map(serde_json::to_value).transpose()?)))
            .map_err(|error| format!("{KIND} {name} at {home}: {error}"));
        Some(encoded.map(|(spec, status)| EngineObject {
            name: name.to_owned(),
            home: home.to_owned(),
            // A change token for the writers' version checks, which read it back from this home: it
            // moves with each write of either record. Delete ends both, and a Host made again
            // starts as a new object.
            version: host_version + held.observation.as_ref().map_or(0, |(_, version)| *version),
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            spec,
            status,
        }))
    }

    /// This home's records of `name`, read from the store rather than the resident map.
    fn own(&self, name: &str) -> Result<Held, String> {
        let host = self.log.get::<HostRecord>(name, &self.home).map_err(|error| error.to_string())?;
        let observation = self.log.get::<HostObservation>(name, &self.home).map_err(|error| error.to_string())?;
        Ok(Held {
            host: host.map(|host| (host.spec, host.version)),
            observation: observation.map(|observation| (observation.spec, observation.version)),
        })
    }

    fn own_object(&self, name: &str) -> Result<EngineObject, String> {
        let held = self.own(name)?;
        self.object(name, &self.home, &held).unwrap_or_else(|| Err(format!("{KIND} {name} is not held at {}", self.home)))
    }

    /// Refused unless a Host `spec` describes is homed here: this daemon's own, or an SSH host it owns.
    fn homed_here(&self, name: &str, spec: &HostSpec) -> Result<(), String> {
        match &spec.connection {
            HostConnection::Daemon if name == self.own_host => Ok(()),
            HostConnection::Daemon => Err(format!("{KIND} {name} is homed at its own daemon; apply it there")),
            HostConnection::SshHost { owning_daemon, .. } if *owning_daemon == self.own_host => Ok(()),
            HostConnection::SshHost { owning_daemon, .. } => {
                Err(format!("{KIND} {name} is homed at its owning daemon `{owning_daemon}`; apply it there"))
            }
        }
    }

    fn change(&self, writer: &str) -> PendingChange {
        PendingChange::new(NewChange::new(writer, Timestamp::from_datetime(&self.now())))
    }

    fn write_spec(&self, name: &str, value: &HostRecord) -> Result<(), String> {
        for _ in 0..WRITE_ATTEMPTS {
            let change = self.change(SPEC_WRITER);
            let written = match self.log.get::<HostRecord>(name, &self.home).map_err(|error| error.to_string())? {
                Some(current) => self.log.edit(name, current.version, value, &change),
                None => self.log.create(name, value, &change),
            };
            match written {
                Ok(_) => return Ok(()),
                Err(PlainError::Conflict { .. } | PlainError::Exists { .. } | PlainError::Missing { .. }) => continue,
                Err(error) => return Err(format!("write {KIND} {name}: {error}")),
            }
        }
        Err(format!("write {KIND} {name}: lost {WRITE_ATTEMPTS} races with concurrent writers"))
    }

    /// Writes this home's observation of `name` from `status`, only where a value changed.
    fn write_observation(&self, name: &str, spec: &HostSpec, status: &HostStatus) -> Result<(), String> {
        for _ in 0..WRITE_ATTEMPTS {
            let held = self.log.get::<HostObservation>(name, &self.home).map_err(|error| error.to_string())?;
            let observation = observed(spec, status, held.as_ref().map(|held| &held.spec))?;
            let change = self.change(OBSERVER);
            let written = match held {
                // An edit that changes nothing logs nothing and opens no change.
                Some(current) => self.log.edit(name, current.version, &observation, &change),
                None => self.log.create(name, &observation, &change),
            };
            match written {
                Ok(_) => return Ok(()),
                Err(PlainError::Conflict { .. } | PlainError::Exists { .. } | PlainError::Missing { .. }) => continue,
                Err(error) => return Err(format!("write {KIND} {name} observation: {error}")),
            }
        }
        Err(format!("write {KIND} {name} observation: lost {WRITE_ATTEMPTS} races with concurrent writers"))
    }
}

fn refuse_kind(kind: &str) -> Result<(), String> {
    if kind == KIND {
        Ok(())
    } else {
        Err(format!("{kind} is not served by the host records"))
    }
}

fn warn_skipped(held: Result<EngineObject, String>) -> Option<EngineObject> {
    held.map_err(|error| tracing::warn!(%error, "skipping a Host that does not encode")).ok()
}

#[async_trait::async_trait]
impl EngineRecords for EngineHosts {
    fn kinds(&self) -> &'static [&'static str] {
        &[KIND]
    }

    fn home(&self) -> String {
        self.home.clone()
    }

    async fn get(&self, kind: &str, name: &str) -> Result<Option<EngineObject>, String> {
        refuse_kind(kind)?;
        let resident = self.refresh()?;
        resolve(&resident.records, &self.home, name).and_then(|(home, held)| self.object(name, home, held)).transpose()
    }

    async fn list(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        refuse_kind(kind)?;
        let resident = self.refresh()?;
        let mut names: Vec<&String> = resident.records.keys().map(|(name, _)| name).collect();
        // The map orders by name, then home, so one name's homes are adjacent.
        names.dedup();
        Ok(names
            .into_iter()
            .filter_map(|name| resolve(&resident.records, &self.home, name).and_then(|(home, held)| self.object(name, home, held)))
            .filter_map(warn_skipped)
            .collect())
    }

    async fn sources(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        refuse_kind(kind)?;
        let resident = self.refresh()?;
        Ok(resident.records.iter().filter_map(|((name, home), held)| self.object(name, home, held)).filter_map(warn_skipped).collect())
    }

    async fn apply(
        &self,
        kind: &str,
        name: &str,
        _labels: &BTreeMap<String, String>,
        _annotations: &BTreeMap<String, String>,
        spec: Value,
    ) -> Result<EngineObject, String> {
        refuse_kind(kind)?;
        let spec: HostSpec = serde_json::from_value(spec).map_err(|error| format!("{KIND} spec: {error}"))?;
        self.homed_here(name, &spec)?;
        let _writing = self.writing.lock().await;
        self.write_spec(name, &HostRecord::new(&spec))?;
        self.own_object(name)
    }

    async fn set_status(&self, kind: &str, name: &str, status: Value) -> Result<EngineObject, String> {
        refuse_kind(kind)?;
        let status: HostStatus = serde_json::from_value(status).map_err(|error| format!("{KIND} status: {error}"))?;
        let _writing = self.writing.lock().await;
        let Some((host, _)) = self.own(name)?.host else {
            return Err(format!("{KIND} {name} is not held at {}", self.home));
        };
        self.write_observation(name, &host.spec(), &status)?;
        self.own_object(name)
    }

    async fn delete(&self, kind: &str, name: &str) -> Result<(), String> {
        refuse_kind(kind)?;
        let _writing = self.writing.lock().await;
        let held = self.own(name)?;
        let change = self.change(SPEC_WRITER);
        let deleted = |result: Result<(), PlainError>| match result {
            Ok(()) | Err(PlainError::Missing { .. }) => Ok(()),
            Err(error) => Err(format!("delete {KIND} {name}: {error}")),
        };
        if let Some((_, version)) = held.observation {
            deleted(self.log.delete::<HostObservation>(name, version, &change))?;
        }
        if let Some((_, version)) = held.host {
            deleted(self.log.delete::<HostRecord>(name, version, &change))?;
        }
        Ok(())
    }

    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let service = Arc::clone(&self.service);
        let mut follower = Follower::new([HOST, OBSERVATION]);
        let homes = service.homes();
        service.with_store(|store| follower.start(&mut store.read()?, &homes)).map_err(|error| error.to_string())?;
        let stream = futures::stream::unfold((service, follower), |(service, mut follower)| async move {
            loop {
                tokio::time::sleep(POLL).await;
                let homes = service.homes();
                match service.with_store(|store| Ok(!follower.pass(&mut store.read()?, &homes)?.is_empty())) {
                    Ok(true) => return Some(((), (service, follower))),
                    Ok(false) => {}
                    Err(error) => tracing::warn!(%error, "host change stream could not read the log; retrying"),
                }
            }
        });
        Ok(Box::pin(stream))
    }
}
