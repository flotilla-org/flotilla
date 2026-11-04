//! One engine port for a plain kind, behind `[primitives]`.
//!
//! [`PlainRecords`] is the [`EngineRecords`] port of a [`PlainKind`] over the decided store. Each
//! record is written at its home through the plain-kind port; every home's are read from a
//! resident map kept current by following the log.

use std::{
    collections::BTreeMap,
    marker::PhantomData,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use flotilla_engine_store::{
    follow::{Followed, Follower},
    plain::{PendingChange, PlainError, PlainLog},
};
use flotilla_resources::engine_records::{EngineObject, EngineRecords};
use flotilla_schema::{Kind, NewChange, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, StoreError};
use futures::stream::BoxStream;
use serde_json::Value;

/// How often a change stream looks for new entries in the log, as the account port's does.
const POLL: Duration = Duration::from_millis(500);

/// How often a write that loses a race to another writer of its name reads and tries again.
const WRITE_ATTEMPTS: usize = 3;

/// One home's record as the resident map holds it: the old object it reads as, or why it does not.
pub type Held = Result<EngineObject, String>;

/// Every home's records, by name and home.
type Records = BTreeMap<(String, String), Held>;

struct Resident {
    follower: Follower,
    records: Records,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A plain kind served as old-store records.
pub trait PlainKind: Kind + PartialEq + Send + Sync + 'static {
    /// The writer recorded on this port's writes.
    const WRITER: &'static str;

    /// The old object `record` reads as, with no status.
    fn object(name: String, home: String, version: u64, record: &Self) -> Held;

    /// The record an old object's spec writes.
    fn from_spec(spec: Value) -> Result<Self, String>;
}

fn reload<K: PlainKind>(read: &mut Read<'_>, records: &mut Records, name: &str, home: &str) -> Result<(), StoreError> {
    let key = (name.to_owned(), home.to_owned());
    match read.resolve(K::CODE, name, home)?.map(|id| read.load::<K>(id)).transpose()?.flatten() {
        Some(loaded) => records.insert(key, K::object(loaded.name, loaded.home, loaded.version, &loaded.value)),
        None => records.remove(&key),
    };
    Ok(())
}

fn rescan<K: PlainKind>(read: &mut Read<'_>, records: &mut Records, home: &str) -> Result<(), StoreError> {
    records.retain(|(_, owner), _| owner != home);
    let mut after = None;
    loop {
        let page = read.scan::<K>(home, after, 64)?;
        for id in page.ids {
            if let Some(loaded) = read.load::<K>(id)? {
                let key = (loaded.name.clone(), home.to_owned());
                records.insert(key, K::object(loaded.name, loaded.home, loaded.version, &loaded.value));
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

/// The records of one plain kind in the decided store at one home.
pub struct PlainRecords<K> {
    service: Arc<Service>,
    home: String,
    log: PlainLog,
    resident: Mutex<Resident>,
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
            service,
            home,
            log,
            resident: Mutex::new(Resident { follower: Follower::new([K::CODE]), records: Records::new() }),
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
                            Followed::Changed(identity) if identity.kind == K::CODE => {
                                reload::<K>(&mut read, records, &identity.name, &identity.home)?
                            }
                            Followed::Changed(_) => {}
                            Followed::Rescan(home) => rescan::<K>(&mut read, records, &home)?,
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
    fn write(&self, name: &str, value: &K) -> Result<EngineObject, String> {
        let mut lost = None;
        for _ in 0..WRITE_ATTEMPTS {
            let current = self.log.get::<K>(name, &self.home).map_err(|error| error.to_string())?;
            let written = match current {
                Some(current) => self.log.edit(name, current.version, value, &PendingChange::new(NewChange::new(K::WRITER, self.now()))),
                None => self.log.create(name, value, &PendingChange::new(NewChange::new(K::WRITER, self.now()))),
            };
            match written {
                Ok(version) => return K::object(name.to_owned(), self.home.clone(), version, value),
                Err(error @ (PlainError::Conflict { .. } | PlainError::Exists { .. } | PlainError::Missing { .. })) => lost = Some(error),
                Err(error) => return Err(format!("write {} {name}: {error}", K::KIND)),
            }
        }
        let last = lost.map(|error| error.to_string()).unwrap_or_default();
        Err(format!("write {} {name}: lost {WRITE_ATTEMPTS} races with concurrent writers, the last: {last}", K::KIND))
    }
}

fn refuse_kind<K: PlainKind>(kind: &str) -> Result<(), String> {
    if kind == K::KIND {
        Ok(())
    } else {
        Err(format!("{kind} is not served by the {} records", K::KIND))
    }
}

#[async_trait::async_trait]
impl<K: PlainKind> EngineRecords for PlainRecords<K> {
    fn kinds(&self) -> &'static [&'static str] {
        &[K::KIND]
    }

    fn home(&self) -> String {
        self.home.clone()
    }

    async fn get(&self, kind: &str, name: &str) -> Result<Option<EngineObject>, String> {
        refuse_kind::<K>(kind)?;
        let resident = self.refresh()?;
        resolve(&resident.records, &self.home, name).transpose()
    }

    async fn list(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        refuse_kind::<K>(kind)?;
        let resident = self.refresh()?;
        let mut names: Vec<&String> = resident.records.keys().map(|(name, _)| name).collect();
        // The map orders by name, then home, so one name's homes are adjacent.
        names.dedup();
        Ok(names
            .into_iter()
            .filter_map(|name| resolve(&resident.records, &self.home, name))
            .filter_map(|held| held.map_err(|error| tracing::warn!(%error, kind = K::KIND, "skipping a record that does not encode")).ok())
            .collect())
    }

    async fn sources(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        refuse_kind::<K>(kind)?;
        let resident = self.refresh()?;
        Ok(resident
            .records
            .values()
            .filter_map(|held| {
                held.clone().map_err(|error| tracing::warn!(%error, kind = K::KIND, "skipping a record that does not encode")).ok()
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
        refuse_kind::<K>(kind)?;
        self.write(name, &K::from_spec(spec)?)
    }

    async fn set_status(&self, kind: &str, name: &str, _status: Value) -> Result<EngineObject, String> {
        refuse_kind::<K>(kind)?;
        Err(format!("{} {name} keeps no status", K::KIND))
    }

    async fn delete(&self, kind: &str, name: &str) -> Result<(), String> {
        refuse_kind::<K>(kind)?;
        let Some(current) = self.log.get::<K>(name, &self.home).map_err(|error| error.to_string())? else { return Ok(()) };
        match self.log.delete::<K>(name, current.version, &PendingChange::new(NewChange::new(K::WRITER, self.now()))) {
            Ok(()) | Err(PlainError::Missing { .. }) => Ok(()),
            Err(error) => Err(format!("delete {} {name}: {error}", K::KIND)),
        }
    }

    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let service = Arc::clone(&self.service);
        let mut follower = Follower::new([K::CODE]);
        let homes = service.homes();
        service.with_store(|store| follower.start(&mut store.read()?, &homes)).map_err(|error| error.to_string())?;
        let stream = futures::stream::unfold((service, follower), |(service, mut follower)| async move {
            loop {
                tokio::time::sleep(POLL).await;
                let homes = service.homes();
                match service.with_store(|store| Ok(!follower.pass(&mut store.read()?, &homes)?.is_empty())) {
                    Ok(true) => return Some(((), (service, follower))),
                    Ok(false) => {}
                    Err(error) => tracing::warn!(%error, kind = K::KIND, "change stream could not read the log; retrying"),
                }
            }
        });
        Ok(Box::pin(stream))
    }
}
