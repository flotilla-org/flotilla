//! AccessDelivery on the engine, behind `[primitives]`.
//!
//! [`EngineAccessDeliveries`] is the [`EngineRecords`] port of AccessDelivery over the decided
//! store. Each record is written at the home that launched the access, through the plain-kind
//! port; every home's are read from a resident map kept current by following the log. The
//! convoy label old readers select by is read back from the access key, as the table keeps none.

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
    access_delivery::convoy_of_access,
    engine_records::{EngineObject, EngineRecords},
    AccessDeliverySpec, CONVOY_LABEL,
};
use flotilla_schema::{Kind, KindCode, NewChange, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, StoreError};
use futures::stream::BoxStream;
use serde_json::Value;

const KIND: &str = "AccessDelivery";
const CODE: KindCode = <AccessDeliverySpec as Kind>::CODE;

/// How often a change stream looks for new entries in the log, as the account port's does.
const POLL: Duration = Duration::from_millis(500);

/// How often a write that loses a race to another writer of its name reads and tries again.
const WRITE_ATTEMPTS: usize = 3;

/// The writer the plain-kind port records for this port's writes.
const WRITER: &str = "access-delivery-records";

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
fn object(name: String, home: String, version: u64, record: &AccessDeliverySpec) -> Held {
    let labels = convoy_of_access(&record.access).map(|convoy| (CONVOY_LABEL.to_owned(), convoy.to_owned())).into_iter().collect();
    Ok(EngineObject {
        spec: serde_json::to_value(record).map_err(|error| format!("{KIND} {name} at {home}: {error}"))?,
        status: None,
        name,
        home,
        version,
        labels,
        annotations: BTreeMap::new(),
    })
}

fn reload(read: &mut Read<'_>, records: &mut Records, name: &str, home: &str) -> Result<(), StoreError> {
    let key = (name.to_owned(), home.to_owned());
    match read.resolve(CODE, name, home)?.map(|id| read.load::<AccessDeliverySpec>(id)).transpose()?.flatten() {
        Some(loaded) => records.insert(key, object(loaded.name, loaded.home, loaded.version, &loaded.value)),
        None => records.remove(&key),
    };
    Ok(())
}

fn rescan(read: &mut Read<'_>, records: &mut Records, home: &str) -> Result<(), StoreError> {
    records.retain(|(_, owner), _| owner != home);
    let mut after = None;
    loop {
        let page = read.scan::<AccessDeliverySpec>(home, after, 64)?;
        for id in page.ids {
            if let Some(loaded) = read.load::<AccessDeliverySpec>(id)? {
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

/// The access delivery records of the decided store at one home.
pub struct EngineAccessDeliveries {
    service: Arc<Service>,
    home: String,
    log: PlainLog,
    resident: Mutex<Resident>,
    clock: Arc<dyn flotilla_resources::Clock>,
}

impl std::fmt::Debug for EngineAccessDeliveries {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("EngineAccessDeliveries").field("home", &self.home).finish_non_exhaustive()
    }
}

impl EngineAccessDeliveries {
    /// Opens this home's access delivery writer over `service`, and reads every home's records.
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
    fn write(&self, name: &str, value: &AccessDeliverySpec) -> Result<EngineObject, String> {
        let mut lost = None;
        for _ in 0..WRITE_ATTEMPTS {
            let current = self.log.get::<AccessDeliverySpec>(name, &self.home).map_err(|error| error.to_string())?;
            let written = match current {
                Some(current) => self.log.edit(name, current.version, value, &PendingChange::new(NewChange::new(WRITER, self.now()))),
                None => self.log.create(name, value, &PendingChange::new(NewChange::new(WRITER, self.now()))),
            };
            match written {
                Ok(version) => return object(name.to_owned(), self.home.clone(), version, value),
                Err(error @ (PlainError::Conflict { .. } | PlainError::Exists { .. } | PlainError::Missing { .. })) => lost = Some(error),
                Err(error) => return Err(format!("write {KIND} {name}: {error}")),
            }
        }
        let last = lost.map(|error| error.to_string()).unwrap_or_default();
        Err(format!("write {KIND} {name}: lost {WRITE_ATTEMPTS} races with concurrent writers, the last: {last}"))
    }
}

fn refuse_kind(kind: &str) -> Result<(), String> {
    if kind == KIND {
        Ok(())
    } else {
        Err(format!("{kind} is not served by the access delivery records"))
    }
}

#[async_trait::async_trait]
impl EngineRecords for EngineAccessDeliveries {
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
            .filter_map(|held| held.map_err(|error| tracing::warn!(%error, "skipping an access delivery record that does not encode")).ok())
            .collect())
    }

    async fn sources(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        refuse_kind(kind)?;
        let resident = self.refresh()?;
        Ok(resident
            .records
            .values()
            .filter_map(|held| {
                held.clone().map_err(|error| tracing::warn!(%error, "skipping an access delivery record that does not encode")).ok()
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
        let spec: AccessDeliverySpec = serde_json::from_value(spec).map_err(|error| format!("{KIND} spec: {error}"))?;
        self.write(name, &spec)
    }

    async fn set_status(&self, kind: &str, name: &str, _status: Value) -> Result<EngineObject, String> {
        refuse_kind(kind)?;
        Err(format!("{KIND} {name} keeps no status"))
    }

    async fn delete(&self, kind: &str, name: &str) -> Result<(), String> {
        refuse_kind(kind)?;
        let Some(current) = self.log.get::<AccessDeliverySpec>(name, &self.home).map_err(|error| error.to_string())? else { return Ok(()) };
        match self.log.delete::<AccessDeliverySpec>(name, current.version, &PendingChange::new(NewChange::new(WRITER, self.now()))) {
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
                    Err(error) => tracing::warn!(%error, "access delivery change stream could not read the log; retrying"),
                }
            }
        });
        Ok(Box::pin(stream))
    }
}
