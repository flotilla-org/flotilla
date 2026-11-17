//! One engine port for a plain kind, behind `[primitives]`.
//!
//! [`PlainRecords`] is the [`KindRecords`] port of a [`PlainKind`] over the decided store. Each
//! record is written at its home through the plain-kind port; every home's are read from a
//! resident map kept current by following the log.

use std::{
    collections::BTreeMap,
    marker::PhantomData,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use super::reads::created_at;
use super::resume::logged_since;
use super::watch::CommitsSeen;
use super::watch::SharedChanges;
use chrono::{DateTime, Utc};
use flotilla_engine_store::{
    follow::{Followed, Follower},
    plain::{PendingChange, PlainError, PlainLog},
};
use flotilla_resources::{
    engine_records::{CommitMark, KindRecords, LogPosition, Record, Since, Ticks, WriteError},
    Resource,
};
use flotilla_schema::{Kind, NewChange, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, StoreError};
use futures::stream::BoxStream;

/// How often a write that loses a race to another writer of its name reads and tries again.
const WRITE_ATTEMPTS: usize = 3;

/// One home's record as the resident map holds it: the resource it reads as, or why it does not.
type Held<K> = Result<Record<<K as PlainKind>::Resource>, String>;

/// Every home's records, by name and home.
type Records<K> = BTreeMap<(String, String), Held<K>>;

struct Resident<K: PlainKind> {
    follower: Follower,
    records: Records<K>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A plain kind served as resource records.
pub trait PlainKind: Kind + PartialEq + Send + Sync + 'static {
    /// The resource kind it is served as.
    type Resource: Resource;

    /// The writer recorded on this port's writes.
    const WRITER: &'static str;

    /// The spec the record reads as.
    fn spec(&self) -> Result<<Self::Resource as Resource>::Spec, String>;

    /// The labels the record reads as, read back from what it holds; none unless it says.
    fn labels(&self) -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    /// The record a spec writes.
    fn from_spec(spec: <Self::Resource as Resource>::Spec) -> Result<Self, String>;
}

/// `record`, created at `created_at`, as the resource, with no status.
fn object<K: PlainKind>(name: String, home: String, version: u64, created_at: DateTime<Utc>, record: &K) -> Held<K> {
    let spec = record.spec().map_err(|error| format!("{} {name} at {home}: {error}", <K as Kind>::KIND))?;
    let labels = record.labels();
    Ok(Record { name, home, version, created_at, labels, annotations: BTreeMap::new(), spec, status: None })
}

fn reload<K: PlainKind>(read: &mut Read<'_>, records: &mut Records<K>, name: &str, home: &str) -> Result<(), StoreError> {
    let key = (name.to_owned(), home.to_owned());
    match read.resolve(K::CODE, name, home)?.map(|id| read.load::<K>(id)).transpose()?.flatten() {
        Some(loaded) => {
            let created_at = created_at(read, loaded.id)?;
            records.insert(key, object(loaded.name, loaded.home, loaded.version, created_at, &loaded.value))
        }
        None => records.remove(&key),
    };
    Ok(())
}

fn rescan<K: PlainKind>(read: &mut Read<'_>, records: &mut Records<K>, home: &str) -> Result<(), StoreError> {
    records.retain(|(_, owner), _| owner != home);
    let mut after = None;
    loop {
        let page = read.scan::<K>(home, after, 64)?;
        for id in page.ids {
            if let Some(loaded) = read.load::<K>(id)? {
                let key = (loaded.name.clone(), home.to_owned());
                let created_at = created_at(read, id)?;
                records.insert(key, object(loaded.name, loaded.home, loaded.version, created_at, &loaded.value));
            }
        }
        after = page.next;
        if after.is_none() {
            return Ok(());
        }
    }
}

/// `name`'s record as several homes' resolve at `home`: its own, else the lexically first home's.
fn resolve<K: PlainKind>(records: &Records<K>, home: &str, name: &str) -> Option<Held<K>> {
    let mut held = records.range((name.to_owned(), String::new())..).take_while(|((held, _), _)| held == name);
    let first = held.next()?;
    let chosen = std::iter::once(first).chain(held).find(|((_, owner), _)| owner == home).unwrap_or(first);
    Some(chosen.1.clone())
}

/// The records of one plain kind in the decided store at one home.
pub struct PlainRecords<K: PlainKind> {
    service: Arc<Service>,
    home: String,
    log: PlainLog,
    resident: Mutex<Resident<K>>,
    /// Whether the store has committed since the resident map was last brought up to it.
    commits: CommitsSeen,
    // One follower for every watcher of these records on this daemon.
    changes: SharedChanges,
    clock: Arc<dyn flotilla_resources::Clock>,
    kind: PhantomData<fn() -> K>,
}

impl<K: PlainKind> std::fmt::Debug for PlainRecords<K> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PlainRecords").field("home", &self.home).finish_non_exhaustive()
    }
}

impl<K: PlainKind> PlainRecords<K> {
    /// Opens this home's writer over `service`, and reads every home's records.
    pub fn open(service: Arc<Service>, clock: Arc<dyn flotilla_resources::Clock>) -> Result<Self, String> {
        let log = PlainLog::new(service.store());
        let home = log.home().to_owned();
        let opened = Self {
            changes: SharedChanges::new(Arc::clone(&service), [K::CODE], K::KIND),
            commits: CommitsSeen::follow(&service, [K::CODE]),
            service,
            home,
            log,
            resident: Mutex::new(Resident { follower: Follower::new([K::CODE]), records: Records::<K>::new() }),
            clock,
            kind: PhantomData,
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
                        rescan::<K>(&mut read, records, home)?;
                    }
                    Ok(())
                })
                .map_err(|error| error.to_string())?;
        }
        Ok(opened)
    }

    /// Brings the resident map up to the store: every entry written since the last refresh.
    fn refresh(&self) -> Result<MutexGuard<'_, Resident<K>>, String> {
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
                            Followed::Changed(identity) if identity.kind == K::CODE => {
                                reload::<K>(&mut read, records, &identity.name, &identity.home)?
                            }
                            Followed::Changed(_) => {}
                            Followed::Rescan(home) => rescan::<K>(&mut read, records, &home)?,
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

    /// Writes this home's record `name` as `value`, created or replaced, re-reading and trying
    /// again when another writer of the name wins the race.
    /// Refused as stale when a `version` is given and the record has moved from it: the log's
    /// edit at the version read checks it again.
    fn write(&self, name: &str, version: Option<u64>, value: &K) -> Result<Record<K::Resource>, WriteError> {
        let mut lost = None;
        for _ in 0..WRITE_ATTEMPTS {
            let current = self.log.get::<K>(name, &self.home).map_err(|error| error.to_string())?;
            WriteError::unless_at(version, current.as_ref().map(|current| current.version))?;
            let written = match current {
                Some(current) => self.log.edit(name, current.version, value, &PendingChange::new(NewChange::new(K::WRITER, self.now()))),
                None => self.log.create(name, value, &PendingChange::new(NewChange::new(K::WRITER, self.now()))),
            };
            match written {
                Ok(written) => {
                    let created_at = self.log.created::<K>(name, &self.home).map_err(|error| error.to_string())?.to_datetime();
                    return Ok(object(name.to_owned(), self.home.clone(), written, created_at, value)?);
                }
                Err(error @ (PlainError::Conflict { .. } | PlainError::Exists { .. } | PlainError::Missing { .. })) => lost = Some(error),
                Err(error) => return Err(format!("write {} {name}: {error}", K::KIND).into()),
            }
        }
        let last = lost.map(|error| error.to_string()).unwrap_or_default();
        Err(format!("write {} {name}: lost {WRITE_ATTEMPTS} races with concurrent writers, the last: {last}", K::KIND).into())
    }
}

impl<K: PlainKind> flotilla_store::EnginePort for PlainRecords<K> {
    fn install(self: Arc<Self>, backend: &flotilla_store::ResourceBackend) {
        backend.install_engine_kind::<K::Resource>(self);
    }
}

#[async_trait::async_trait]
impl<K: PlainKind> KindRecords<K::Resource> for PlainRecords<K> {
    fn codes(&self) -> Vec<flotilla_schema::KindCode> {
        vec![K::CODE]
    }

    /// From the shared follower of the log.
    fn ticks(&self) -> Ticks {
        Ticks::Followed
    }

    fn commit_mark(&self) -> Option<CommitMark> {
        Some(super::commit_mark(&self.service, self.codes()))
    }

    fn home(&self) -> String {
        self.home.clone()
    }

    async fn get(&self, name: &str) -> Result<Option<Record<K::Resource>>, String> {
        let resident = self.refresh()?;
        resolve::<K>(&resident.records, &self.home, name).transpose()
    }

    async fn list(&self) -> Result<Vec<Record<K::Resource>>, String> {
        let resident = self.refresh()?;
        let mut names: Vec<&String> = resident.records.keys().map(|(name, _)| name).collect();
        // The map orders by name, then home, so one name's homes are adjacent.
        names.dedup();
        Ok(names
            .into_iter()
            .filter_map(|name| resolve::<K>(&resident.records, &self.home, name))
            .filter_map(|held| held.map_err(|error| tracing::warn!(%error, kind = K::KIND, "skipping a record that does not encode")).ok())
            .collect())
    }

    async fn sources(&self) -> Result<Vec<Record<K::Resource>>, String> {
        let resident = self.refresh()?;
        Ok(resident
            .records
            .values()
            .filter_map(|held| {
                held.clone().map_err(|error| tracing::warn!(%error, kind = K::KIND, "skipping a record that does not encode")).ok()
            })
            .collect())
    }

    async fn positioned(&self) -> Result<(Vec<Record<K::Resource>>, LogPosition), String> {
        let resident = self.refresh()?;
        let records = resident
            .records
            .values()
            .filter_map(|held| {
                held.clone().map_err(|error| tracing::warn!(%error, kind = K::KIND, "skipping a record that does not encode")).ok()
            })
            .collect();
        Ok((records, LogPosition::logged(resident.follower.cursors().clone())))
    }

    async fn position(&self) -> Result<LogPosition, String> {
        Ok(LogPosition::logged(self.refresh()?.follower.cursors().clone()))
    }

    async fn changed_since(&self, position: &LogPosition) -> Result<Since, String> {
        logged_since(&self.service, [K::CODE], position)
    }

    /// Every home's record `name`, read from the resident map by name.
    async fn copies(&self, name: &str) -> Result<Vec<Record<K::Resource>>, String> {
        let resident = self.refresh()?;
        Ok(resident
            .records
            .range((name.to_owned(), String::new())..)
            .take_while(|((held, _), _)| held == name)
            .filter_map(|(_, held)| {
                held.clone().map_err(|error| tracing::warn!(%error, kind = K::KIND, "skipping a record that does not encode")).ok()
            })
            .collect())
    }

    async fn apply(
        &self,
        name: &str,
        version: Option<u64>,
        _labels: &BTreeMap<String, String>,
        _annotations: &BTreeMap<String, String>,
        spec: &<K::Resource as Resource>::Spec,
    ) -> Result<Record<K::Resource>, WriteError> {
        self.write(name, version, &K::from_spec(spec.clone())?)
    }

    async fn set_status(
        &self,
        name: &str,
        _version: u64,
        _status: &<K::Resource as Resource>::Status,
    ) -> Result<Record<K::Resource>, WriteError> {
        Err(format!("{} {name} keeps no status", K::KIND).into())
    }

    async fn delete(&self, name: &str) -> Result<(), String> {
        let Some(current) = self.log.get::<K>(name, &self.home).map_err(|error| error.to_string())? else { return Ok(()) };
        match self.log.delete::<K>(name, current.version, &PendingChange::new(NewChange::new(K::WRITER, self.now()))) {
            Ok(()) | Err(PlainError::Missing { .. }) => Ok(()),
            Err(error) => Err(format!("delete {} {name}: {error}", K::KIND)),
        }
    }

    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        self.changes.subscribe().map_err(|error| error.to_string())
    }
}
