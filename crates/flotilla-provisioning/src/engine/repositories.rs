//! Repository on the engine, behind `[primitives]` (#3824 layer 3a).
//!
//! [`EngineRepositories`] is the [`EngineRecords`] port of Repository over the decided store. Each
//! home writes its own record of each repository it observes, through the plain-kind port; every
//! home's are read from a resident map kept current by following the log.
//!
//! A record reads as the old object: the verification project and provenance and the superseding
//! record as the annotations their writers stamp, and the observed default branch as the status.

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
    definition_provenance::{SUPERSEDED_BY_ANNOTATION, VERIFICATION_PROJECT_ANNOTATION, VERIFICATION_PROVENANCE_ANNOTATION},
    engine_records::{EngineObject, EngineRecords},
    RepositoryKey, RepositorySpec, RepositoryStatus,
};
use flotilla_schema::{Kind, KindCode, NewChange, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, StoreError};
use futures::stream::BoxStream;
use serde_json::Value;

use crate::stored::repository::{RepositoryRecord, VerificationProvenance};

const KIND: &str = "Repository";
const CODE: KindCode = <RepositoryRecord as Kind>::CODE;

/// How often a change stream looks for new entries in the log, as the account port's does.
const POLL: Duration = Duration::from_millis(500);

/// How often a write that loses a race to another writer of its name reads and tries again.
const WRITE_ATTEMPTS: usize = 3;

/// The writer the plain-kind port records for this port's writes.
const WRITER: &str = "repository-records";

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

/// `record` as the old object: its ops data as annotations, its observations as the status.
fn object(name: String, home: String, version: u64, record: &RepositoryRecord) -> Held {
    let encode = |error: serde_json::Error| format!("{KIND} {name} at {home}: {error}");
    let mut annotations = BTreeMap::new();
    if let Some(project) = &record.verification_project {
        annotations.insert(VERIFICATION_PROJECT_ANNOTATION.to_owned(), project.clone());
    }
    if !record.verification_provenance.is_empty() {
        annotations
            .insert(VERIFICATION_PROVENANCE_ANNOTATION.to_owned(), serde_json::to_string(&record.verification_provenance).map_err(encode)?);
    }
    if let Some(target) = &record.superseded_by {
        annotations.insert(SUPERSEDED_BY_ANNOTATION.to_owned(), target.clone());
    }
    let status = RepositoryStatus {
        default_branch: record.default_branch.clone(),
        default_branch_observations: record.default_branch_observations.clone(),
        ..Default::default()
    };
    Ok(EngineObject {
        spec: serde_json::to_value(record.spec()).map_err(encode)?,
        status: Some(serde_json::to_value(status).map_err(encode)?),
        name,
        home,
        version,
        labels: BTreeMap::new(),
        annotations,
    })
}

fn reload(read: &mut Read<'_>, records: &mut Records, name: &str, home: &str) -> Result<(), StoreError> {
    let key = (name.to_owned(), home.to_owned());
    match read.resolve(CODE, name, home)?.map(|id| read.load::<RepositoryRecord>(id)).transpose()?.flatten() {
        Some(loaded) => records.insert(key, object(loaded.name, loaded.home, loaded.version, &loaded.value)),
        None => records.remove(&key),
    };
    Ok(())
}

fn rescan(read: &mut Read<'_>, records: &mut Records, home: &str) -> Result<(), StoreError> {
    records.retain(|(_, owner), _| owner != home);
    let mut after = None;
    loop {
        let page = read.scan::<RepositoryRecord>(home, after, 64)?;
        for id in page.ids {
            if let Some(loaded) = read.load::<RepositoryRecord>(id)? {
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

/// The repository records of the decided store at one home.
pub struct EngineRepositories {
    service: Arc<Service>,
    home: String,
    log: PlainLog,
    resident: Mutex<Resident>,
    clock: Arc<dyn flotilla_resources::Clock>,
}

impl std::fmt::Debug for EngineRepositories {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("EngineRepositories").field("home", &self.home).finish_non_exhaustive()
    }
}

impl EngineRepositories {
    /// Opens this home's repository writer over `service`, and reads every home's records.
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

    /// Writes this home's record `name` as `edit` makes it from the current one (or none),
    /// re-reading and trying again when another writer of the name wins the race.
    fn write(
        &self,
        name: &str,
        edit: impl Fn(Option<RepositoryRecord>) -> Result<RepositoryRecord, String>,
    ) -> Result<EngineObject, String> {
        for _ in 0..WRITE_ATTEMPTS {
            let current = self.log.get::<RepositoryRecord>(name, &self.home).map_err(|error| error.to_string())?;
            let version = current.as_ref().map(|current| current.version);
            let value = edit(current.as_ref().map(|current| current.spec.clone()))?;
            let written = match (current, version) {
                (Some(current), Some(version)) if current.spec == value => Ok(version),
                (Some(_), Some(version)) => self.log.edit(name, version, &value, &PendingChange::new(NewChange::new(WRITER, self.now()))),
                _ => self.log.create(name, &value, &PendingChange::new(NewChange::new(WRITER, self.now()))),
            };
            match written {
                Ok(version) => return object(name.to_owned(), self.home.clone(), version, &value),
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
        Err(format!("{kind} is not served by the repository records"))
    }
}

#[async_trait::async_trait]
impl EngineRecords for EngineRepositories {
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
            .filter_map(|held| held.map_err(|error| tracing::warn!(%error, "skipping a repository record that does not encode")).ok())
            .collect())
    }

    async fn sources(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        refuse_kind(kind)?;
        let resident = self.refresh()?;
        Ok(resident
            .records
            .values()
            .filter_map(|held| {
                held.clone().map_err(|error| tracing::warn!(%error, "skipping a repository record that does not encode")).ok()
            })
            .collect())
    }

    async fn apply(
        &self,
        kind: &str,
        name: &str,
        _labels: &BTreeMap<String, String>,
        annotations: &BTreeMap<String, String>,
        spec: Value,
    ) -> Result<EngineObject, String> {
        refuse_kind(kind)?;
        let spec: RepositorySpec = serde_json::from_value(spec).map_err(|error| format!("{KIND} spec: {error}"))?;
        spec.verify_key(&RepositoryKey(name.to_owned()))?;
        let verification_provenance = match annotations.get(VERIFICATION_PROVENANCE_ANNOTATION) {
            Some(json) => serde_json::from_str::<Vec<VerificationProvenance>>(json)
                .map_err(|error| format!("`{VERIFICATION_PROVENANCE_ANNOTATION}` does not decode: {error}"))?,
            None => Vec::new(),
        };
        self.write(name, |current| {
            // The observations stay: they are the reconciler's, written through the status.
            let mut record = RepositoryRecord::new(spec.clone())?;
            record.verification_project = annotations.get(VERIFICATION_PROJECT_ANNOTATION).cloned();
            record.verification_provenance.clone_from(&verification_provenance);
            record.superseded_by = annotations.get(SUPERSEDED_BY_ANNOTATION).cloned();
            if let Some(current) = current {
                record.default_branch = current.default_branch;
                record.default_branch_observations = current.default_branch_observations;
            }
            Ok(record)
        })
    }

    async fn set_status(&self, kind: &str, name: &str, status: Value) -> Result<EngineObject, String> {
        refuse_kind(kind)?;
        let status: RepositoryStatus = serde_json::from_value(status).map_err(|error| format!("{KIND} status: {error}"))?;
        self.write(name, |current| {
            let mut record = current.ok_or_else(|| format!("{KIND} {name} is not held here"))?;
            // Checkouts by host and diagnostics have no reader and are not kept (#3824 layer 3a).
            record.default_branch.clone_from(&status.default_branch);
            record.default_branch_observations.clone_from(&status.default_branch_observations);
            Ok(record)
        })
    }

    async fn delete(&self, kind: &str, name: &str) -> Result<(), String> {
        refuse_kind(kind)?;
        let Some(current) = self.log.get::<RepositoryRecord>(name, &self.home).map_err(|error| error.to_string())? else { return Ok(()) };
        match self.log.delete::<RepositoryRecord>(name, current.version, &PendingChange::new(NewChange::new(WRITER, self.now()))) {
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
                    Err(error) => tracing::warn!(%error, "repository change stream could not read the log; retrying"),
                }
            }
        });
        Ok(Box::pin(stream))
    }
}
