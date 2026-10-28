//! Artifact on the engine, behind `[primitives]`.
//!
//! [`EngineArtifacts`] is the [`EngineRecords`] port of Artifact over the decided store. Each
//! artifact is written at its home, the convoy's home for a crew's put and the builder's for a
//! build log, through the plain-kind port; every home's are read from a resident map kept current
//! by following the log.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use flotilla_engine_store::{
    follow::{Followed, Follower},
    plain::{PendingChange, PlainError, PlainLog},
};
use flotilla_resources::{
    engine_records::{EngineObject, EngineRecords},
    ArtifactSpec,
};
use flotilla_schema::{Kind, KindCode, NewChange, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, StoreError};
use futures::stream::BoxStream;
use serde_json::Value;

use crate::stored::artifact::ArtifactRecord;

const KIND: &str = "Artifact";
const CODE: KindCode = <ArtifactRecord as Kind>::CODE;

/// How often a change stream looks for new entries in the log, as the account port's does.
const POLL: Duration = Duration::from_millis(500);

/// How often a write that loses a race to another writer of its name reads and tries again.
const WRITE_ATTEMPTS: usize = 3;

/// The writer the plain-kind port records for this port's writes.
const WRITER: &str = "artifact-records";

/// One home's record as the resident map holds it: the old object it reads as, or why it does not.
type Held = Result<EngineObject, String>;

/// Every home's records, by name and home.
type Records = BTreeMap<(String, String), Held>;

struct Resident {
    follower: Follower,
    records: Records,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `record` as the old object, with no status.
fn object(name: String, home: String, version: u64, record: &ArtifactRecord) -> Held {
    let spec = record.spec().map_err(|error| format!("{KIND} {name} at {home}: {error}"))?;
    Ok(EngineObject {
        spec: serde_json::to_value(spec).map_err(|error| format!("{KIND} {name} at {home}: {error}"))?,
        status: None,
        name,
        home,
        version,
        labels: BTreeMap::new(),
        annotations: BTreeMap::new(),
    })
}

fn reload(read: &mut Read<'_>, records: &mut Records, name: &str, home: &str) -> Result<(), StoreError> {
    let key = (name.to_owned(), home.to_owned());
    match read.resolve(CODE, name, home)?.map(|id| read.load::<ArtifactRecord>(id)).transpose()?.flatten() {
        Some(loaded) => records.insert(key, object(loaded.name, loaded.home, loaded.version, &loaded.value)),
        None => records.remove(&key),
    };
    Ok(())
}

fn rescan(read: &mut Read<'_>, records: &mut Records, home: &str) -> Result<(), StoreError> {
    records.retain(|(_, owner), _| owner != home);
    let mut after = None;
    loop {
        let page = read.scan::<ArtifactRecord>(home, after, 64)?;
        for id in page.ids {
            if let Some(loaded) = read.load::<ArtifactRecord>(id)? {
                let key = (loaded.name.clone(), home.to_owned());
                records.insert(key, object(loaded.name, loaded.home, loaded.version, &loaded.value));
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

/// The artifact records of the decided store at one home.
pub struct EngineArtifacts {
    service: Arc<Service>,
    home: String,
    log: PlainLog,
    resident: Mutex<Resident>,
    clock: Arc<dyn flotilla_resources::Clock>,
}

impl std::fmt::Debug for EngineArtifacts {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("EngineArtifacts").field("home", &self.home).finish_non_exhaustive()
    }
}

impl EngineArtifacts {
    /// Opens this home's artifact writer over `service`, and reads every home's records.
    pub fn open(service: Arc<Service>, clock: Arc<dyn flotilla_resources::Clock>) -> Result<Self, String> {
        let log = PlainLog::new(service.store());
        let home = log.home().to_owned();
        let opened =
            Self { service, home, log, resident: Mutex::new(Resident { follower: Follower::new([CODE]), records: Records::new() }), clock };
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
                            Followed::Changed(identity) if identity.kind == CODE => {
                                reload(&mut read, records, &identity.name, &identity.home)?
                            }
                            Followed::Changed(_) => {}
                            Followed::Rescan(home) => rescan(&mut read, records, &home)?,
                        }
                    }
                    Ok(())
                })
                .map_err(|error| error.to_string())?;
        }
        Ok(resident)
    }

    fn now(&self) -> Timestamp {
        Timestamp::from_datetime(&self.clock.now())
    }

    /// Writes this home's record `name` as `value`, created or replaced, re-reading and trying
    /// again when another writer of the name wins the race.
    fn write(&self, name: &str, value: &ArtifactRecord) -> Result<EngineObject, String> {
        for _ in 0..WRITE_ATTEMPTS {
            let current = self.log.get::<ArtifactRecord>(name, &self.home).map_err(|error| error.to_string())?;
            let written = match current {
                Some(current) => self.log.edit(name, current.version, value, &PendingChange::new(NewChange::new(WRITER, self.now()))),
                None => self.log.create(name, value, &PendingChange::new(NewChange::new(WRITER, self.now()))),
            };
            match written {
                Ok(version) => return object(name.to_owned(), self.home.clone(), version, value),
                Err(PlainError::Conflict { .. } | PlainError::Exists { .. } | PlainError::Missing { .. }) => continue,
                Err(error) => return Err(format!("write {KIND} {name}: {error}")),
            }
        }
        Err(format!("write {KIND} {name}: lost {WRITE_ATTEMPTS} races with concurrent writers"))
    }
}

fn refuse_kind(kind: &str) -> Result<(), String> {
    if kind == KIND {
        Ok(())
    } else {
        Err(format!("{kind} is not served by the artifact records"))
    }
}

#[async_trait::async_trait]
impl EngineRecords for EngineArtifacts {
    fn kinds(&self) -> &'static [&'static str] {
        &[KIND]
    }

    fn home(&self) -> String {
        self.home.clone()
    }

    async fn get(&self, kind: &str, name: &str) -> Result<Option<EngineObject>, String> {
        refuse_kind(kind)?;
        let resident = self.refresh()?;
        resolve(&resident.records, &self.home, name).transpose()
    }

    async fn list(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        refuse_kind(kind)?;
        let resident = self.refresh()?;
        let mut names: Vec<&String> = resident.records.keys().map(|(name, _)| name).collect();
        // The map orders by name, then home, so one name's homes are adjacent.
        names.dedup();
        Ok(names
            .into_iter()
            .filter_map(|name| resolve(&resident.records, &self.home, name))
            .filter_map(|held| held.map_err(|error| tracing::warn!(%error, "skipping an artifact record that does not encode")).ok())
            .collect())
    }

    async fn sources(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        refuse_kind(kind)?;
        let resident = self.refresh()?;
        Ok(resident
            .records
            .values()
            .filter_map(|held| {
                held.clone().map_err(|error| tracing::warn!(%error, "skipping an artifact record that does not encode")).ok()
            })
            .collect())
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
        let spec: ArtifactSpec = serde_json::from_value(spec).map_err(|error| format!("{KIND} spec: {error}"))?;
        let record = ArtifactRecord::from_spec(&spec)?;
        self.write(name, &record)
    }

    async fn set_status(&self, kind: &str, name: &str, _status: Value) -> Result<EngineObject, String> {
        refuse_kind(kind)?;
        Err(format!("{KIND} {name} keeps no status"))
    }

    async fn delete(&self, kind: &str, name: &str) -> Result<(), String> {
        refuse_kind(kind)?;
        let Some(current) = self.log.get::<ArtifactRecord>(name, &self.home).map_err(|error| error.to_string())? else { return Ok(()) };
        match self.log.delete::<ArtifactRecord>(name, current.version, &PendingChange::new(NewChange::new(WRITER, self.now()))) {
            Ok(()) | Err(PlainError::Missing { .. }) => Ok(()),
            Err(error) => Err(format!("delete {KIND} {name}: {error}")),
        }
    }

    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let service = Arc::clone(&self.service);
        let mut follower = Follower::new([CODE]);
        let homes = service.homes();
        service.with_store(|store| follower.start(&mut store.read()?, &homes)).map_err(|error| error.to_string())?;
        let stream = futures::stream::unfold((service, follower), |(service, mut follower)| async move {
            loop {
                tokio::time::sleep(POLL).await;
                let homes = service.homes();
                match service.with_store(|store| Ok(!follower.pass(&mut store.read()?, &homes)?.is_empty())) {
                    Ok(true) => return Some(((), (service, follower))),
                    Ok(false) => {}
                    Err(error) => tracing::warn!(%error, "artifact change stream could not read the log; retrying"),
                }
            }
        });
        Ok(Box::pin(stream))
    }
}
