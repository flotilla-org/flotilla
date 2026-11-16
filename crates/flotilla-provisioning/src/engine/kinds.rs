//! FulfilmentKind on the engine, behind `[primitives]`.
//!
//! [`EngineKinds`] is the [`KindRecords`] port of FulfilmentKind over the decided store. This
//! home writes its own kinds through the plain-kind port; every home's are read from a resident map
//! kept current by following the log.
//!
//! A kind holds only its own fields; its writer is its change's. A manifest's or an operator's kind
//! takes the definitions' change ([`provenance`]); a kind the daemon seeds for its host is
//! `seeding`'s; admission's placement snapshot is `admission`'s, with the Convoy it was frozen for
//! as the change's item.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use flotilla_engine_store::{
    follow::{Followed, Follower},
    plain::{PendingChange, PlainError, PlainLog},
};
use flotilla_resources::{
    definition_provenance::SEEDING_MANAGER,
    definitions::{origin, stamp},
    engine_records::{KindRecords, Record},
    FulfilmentKind, FulfilmentKindSpec, CONVOY_LABEL, MANAGED_BY_LABEL, PLACEMENT_SNAPSHOT_KIND, PREPARED_SNAPSHOT_LABEL,
};
use flotilla_schema::{ChangeHeader, ChangeItem, Identity, Kind, KindCode, NewChange, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, ResourceId, StoreError};
use futures::stream::BoxStream;

use super::provenance;
use super::reads::created_at;
use super::watch::CommitsSeen;
use super::watch::SharedChanges;
use crate::stored::{manifest_root::ManifestRoot, placement::FulfilmentKindDefinition};

const KIND: &str = <FulfilmentKindDefinition as Kind>::KIND;
const CODE: KindCode = <FulfilmentKindDefinition as Kind>::CODE;
const CONVOY: KindCode = <flotilla_state_machines::Resource<flotilla_state_machines::machines::convoy::Convoy> as Kind>::CODE;
/// A root's baselines hold the paths its kinds read with, so a root's change re-reads its home's kinds.
const ROOT: KindCode = <flotilla_state_machines::Resource<ManifestRoot> as Kind>::CODE;

const SEEDING: &str = "seeding";

/// How often a write that loses a race to another writer of its name reads and tries again.
const WRITE_ATTEMPTS: usize = 3;

/// The writer of a delete, which names no origin: the port deletes what its callers remove.
const WRITER: &str = "kind-records";

/// One home's record as the resident map holds it.
type Held = Record<FulfilmentKind>;

/// Every home's records, by name and home.
type Records = BTreeMap<(String, String), Held>;

struct Resident {
    follower: Follower,
    records: Records,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn is_snapshot(labels: &BTreeMap<String, String>) -> bool {
    labels.get(PREPARED_SNAPSHOT_LABEL).is_some_and(|value| value == PLACEMENT_SNAPSHOT_KIND)
}

/// The change a writer stamping `labels` and `annotations` makes at `home`.
pub(crate) fn change_of(
    labels: &BTreeMap<String, String>,
    annotations: &BTreeMap<String, String>,
    home: &str,
    at: Timestamp,
) -> Result<NewChange, String> {
    if is_snapshot(labels) {
        let items: Vec<ChangeItem> = labels
            .get(CONVOY_LABEL)
            .map(|convoy| ChangeItem { record: Identity { kind: CONVOY, name: convoy.clone(), home: home.to_owned() }, revision: None })
            .into_iter()
            .collect();
        return Ok(NewChange::new(provenance::ADMISSION, at).with_items(items));
    }
    if labels.get(MANAGED_BY_LABEL).is_some_and(|value| value == SEEDING_MANAGER) {
        return Ok(NewChange::new(SEEDING, at));
    }
    Ok(provenance::change_of(&origin(labels, annotations)?, home, at))
}

/// A record's labels and annotations.
type Stamps = (BTreeMap<String, String>, BTreeMap<String, String>);

/// The stamps record `name`'s last change reads back as.
fn stamps(read: &mut Read<'_>, roots: &mut provenance::Roots, name: &str, change: Option<&ChangeHeader>) -> Result<Stamps, StoreError> {
    match change.map(|change| change.writer.as_str()) {
        Some(provenance::ADMISSION) => {
            let mut labels = BTreeMap::from([(PREPARED_SNAPSHOT_LABEL.to_owned(), PLACEMENT_SNAPSHOT_KIND.to_owned())]);
            if let Some(convoy) = change.and_then(|change| change.items.iter().find(|item| item.record.kind == CONVOY)) {
                labels.insert(CONVOY_LABEL.to_owned(), convoy.record.name.clone());
            }
            Ok((labels, BTreeMap::new()))
        }
        Some(SEEDING) => Ok((BTreeMap::from([(MANAGED_BY_LABEL.to_owned(), SEEDING_MANAGER.to_owned())]), BTreeMap::new())),
        _ => Ok(stamp(&provenance::origin_of(read, roots, KIND, name, change, None)?)),
    }
}

/// Record `id` as the resource, with the stamps its last change says.
fn object(
    read: &mut Read<'_>,
    roots: &mut provenance::Roots,
    id: ResourceId,
    name: String,
    home: String,
    version: u64,
    value: &FulfilmentKindDefinition,
) -> Result<Held, StoreError> {
    let change = read.last_change(id)?;
    let (labels, annotations) = stamps(read, roots, &name, change.as_ref())?;
    let created_at = created_at(read, id)?;
    Ok(Record { name, home, version, created_at, labels, annotations, spec: value.spec(), status: None })
}

fn reload(read: &mut Read<'_>, records: &mut Records, name: &str, home: &str) -> Result<(), StoreError> {
    let key = (name.to_owned(), home.to_owned());
    let found = match read.resolve(CODE, name, home)? {
        Some(id) => match read.load::<FulfilmentKindDefinition>(id)? {
            Some(loaded) => {
                Some(object(read, &mut provenance::Roots::default(), id, loaded.name, loaded.home, loaded.version, &loaded.value)?)
            }
            None => None,
        },
        None => None,
    };
    match found {
        Some(found) => records.insert(key, found),
        None => records.remove(&key),
    };
    Ok(())
}

fn rescan(read: &mut Read<'_>, records: &mut Records, home: &str) -> Result<(), StoreError> {
    records.retain(|(_, owner), _| owner != home);
    let roots = &mut provenance::Roots::default();
    let mut after = None;
    loop {
        let page = read.scan::<FulfilmentKindDefinition>(home, after, 64)?;
        for id in page.ids {
            if let Some(loaded) = read.load::<FulfilmentKindDefinition>(id)? {
                let key = (loaded.name.clone(), home.to_owned());
                records.insert(key, object(read, roots, id, loaded.name, loaded.home, loaded.version, &loaded.value)?);
            }
        }
        after = page.next;
        if after.is_none() {
            return Ok(());
        }
    }
}

/// `name`'s record as several homes' resolve at `home`: its own, else the lexically first home's.
fn resolve(records: &Records, home: &str, name: &str) -> Option<Held> {
    let mut held = records.range((name.to_owned(), String::new())..).take_while(|((held, _), _)| held == name);
    let first = held.next()?;
    let chosen = std::iter::once(first).chain(held).find(|((_, owner), _)| owner == home).unwrap_or(first);
    Some(chosen.1.clone())
}

/// The fulfilment kinds of the decided store at one home.
pub struct EngineKinds {
    service: Arc<Service>,
    home: String,
    log: PlainLog,
    resident: Mutex<Resident>,
    /// Whether the store has committed since the resident map was last brought up to it.
    commits: CommitsSeen,
    // One follower for every watcher of these records on this daemon.
    changes: SharedChanges,
    clock: Arc<dyn flotilla_resources::Clock>,
}

impl std::fmt::Debug for EngineKinds {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("EngineKinds").field("home", &self.home).finish_non_exhaustive()
    }
}

impl EngineKinds {
    /// Opens this home's kind writer over `service`, and reads every home's records.
    pub fn open(service: Arc<Service>, clock: Arc<dyn flotilla_resources::Clock>) -> Result<Self, String> {
        let log = PlainLog::new(service.store());
        let home = log.home().to_owned();
        let follower = Follower::new([CODE, ROOT]);
        let opened = Self {
            changes: SharedChanges::new(Arc::clone(&service), [CODE], "fulfilment kinds"),
            commits: CommitsSeen::follow(&service, [CODE, ROOT]),
            service,
            home,
            log,
            resident: Mutex::new(Resident { follower, records: Records::new() }),
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
        if !self.commits.take() {
            return Ok(resident);
        }
        let homes = self.service.homes();
        {
            let Resident { follower, records } = &mut *resident;
            self.service
                .with_store(|store| {
                    let mut read = store.read()?;
                    for followed in follower.pass(&mut read, &homes)? {
                        match followed {
                            Followed::Changed(identity) if identity.kind == ROOT => rescan(&mut read, records, &identity.home)?,
                            Followed::Changed(identity) => reload(&mut read, records, &identity.name, &identity.home)?,
                            Followed::Rescan(home) => rescan(&mut read, records, &home)?,
                        }
                    }
                    Ok(())
                })
                .map_err(|error| error.to_string())
                .inspect_err(|_| self.commits.give_back())?;
        }
        Ok(resident)
    }

    fn now(&self) -> Timestamp {
        Timestamp::from_datetime(&self.clock.now())
    }

    fn write(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
        annotations: &BTreeMap<String, String>,
        value: &FulfilmentKindDefinition,
    ) -> Result<Held, String> {
        for _ in 0..WRITE_ATTEMPTS {
            let change = PendingChange::joining(change_of(labels, annotations, &self.home, self.now())?);
            let written = match self.log.get::<FulfilmentKindDefinition>(name, &self.home).map_err(|error| error.to_string())? {
                // An unchanged kind written again for the same reason logs nothing; for a new reason
                // it is written again under that change.
                Some(current) if current.spec == *value => {
                    let last = self.log.last_change::<FulfilmentKindDefinition>(name, &self.home).map_err(|error| error.to_string())?;
                    if last.is_some_and(|last| last.writer == change.change().writer && last.items == change.change().items) {
                        Ok(current.version)
                    } else {
                        self.log.touch::<FulfilmentKindDefinition>(name, current.version, &change)
                    }
                }
                Some(current) => self.log.edit(name, current.version, value, &change),
                None => self.log.create(name, value, &change),
            };
            match written {
                Ok(version) => {
                    let created = self.log.created::<FulfilmentKindDefinition>(name, &self.home).map_err(|error| error.to_string())?;
                    return Ok(Record {
                        name: name.to_owned(),
                        home: self.home.clone(),
                        version,
                        created_at: created.to_datetime(),
                        labels: labels.clone(),
                        annotations: annotations.clone(),
                        spec: value.spec(),
                        status: None,
                    });
                }
                Err(PlainError::Conflict { .. } | PlainError::Exists { .. } | PlainError::Missing { .. }) => continue,
                Err(error) => return Err(format!("write {KIND} {name}: {error}")),
            }
        }
        Err(format!("write {KIND} {name}: lost {WRITE_ATTEMPTS} races with concurrent writers"))
    }
}

impl flotilla_store::EnginePort for EngineKinds {
    fn install(self: Arc<Self>, backend: &flotilla_store::ResourceBackend) {
        backend.install_engine_kind::<FulfilmentKind>(self);
    }
}

#[async_trait::async_trait]
impl KindRecords<FulfilmentKind> for EngineKinds {
    fn codes(&self) -> Vec<KindCode> {
        vec![CODE, ROOT]
    }

    fn home(&self) -> String {
        self.home.clone()
    }

    async fn get(&self, name: &str) -> Result<Option<Held>, String> {
        let resident = self.refresh()?;
        Ok(resolve(&resident.records, &self.home, name))
    }

    async fn list(&self) -> Result<Vec<Held>, String> {
        let resident = self.refresh()?;
        let mut names: Vec<&String> = resident.records.keys().map(|(name, _)| name).collect();
        // The map orders by name, then home, so one name's homes are adjacent.
        names.dedup();
        Ok(names.into_iter().filter_map(|name| resolve(&resident.records, &self.home, name)).collect())
    }

    async fn sources(&self) -> Result<Vec<Held>, String> {
        let resident = self.refresh()?;
        Ok(resident.records.values().cloned().collect())
    }

    /// Every home's record `name`, read from the resident map by name.
    async fn copies(&self, name: &str) -> Result<Vec<Held>, String> {
        let resident = self.refresh()?;
        Ok(resident
            .records
            .range((name.to_owned(), String::new())..)
            .take_while(|((held, _), _)| held == name)
            .map(|(_, held)| held.clone())
            .collect())
    }

    async fn apply(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
        annotations: &BTreeMap<String, String>,
        spec: &FulfilmentKindSpec,
    ) -> Result<Held, String> {
        self.write(name, labels, annotations, &FulfilmentKindDefinition::new(spec)?)
    }

    async fn set_status(&self, name: &str, _status: &()) -> Result<Held, String> {
        Err(format!("{KIND} {name}: a fulfilment kind has no status"))
    }

    async fn delete(&self, name: &str) -> Result<(), String> {
        let Some(current) = self.log.get::<FulfilmentKindDefinition>(name, &self.home).map_err(|error| error.to_string())? else {
            return Ok(());
        };
        match self.log.delete::<FulfilmentKindDefinition>(name, current.version, &PendingChange::new(NewChange::new(WRITER, self.now()))) {
            Ok(()) | Err(PlainError::Missing { .. }) => Ok(()),
            Err(error) => Err(format!("delete {KIND} {name}: {error}")),
        }
    }

    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        self.changes.subscribe().map_err(|error| error.to_string())
    }
}
