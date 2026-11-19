//! Repository on the engine, its only path (#3824 layer 3a).
//!
//! [`EngineRepositories`] is the [`KindRecords`] port of Repository over the decided store. Each
//! home writes its own record of each repository it observes, through the plain-kind port; every
//! home's are read from a resident map kept current by following the log.
//!
//! A record reads as the resource: the verification project and provenance and the superseding
//! record as the annotations their writers stamp, and the observed default branch as the status.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use flotilla_engine_store::{
    follow::{Followed, Follower},
    plain::{PendingChange, PlainError, PlainLog},
};
use flotilla_resources::{
    definition_provenance::{SUPERSEDED_BY_ANNOTATION, VERIFICATION_PROJECT_ANNOTATION, VERIFICATION_PROVENANCE_ANNOTATION},
    engine_records::{CommitMark, IndexCover, KindRecords, LogPosition, Record, Since, Ticks, WriteError},
    Repository, RepositoryKey, RepositorySpec, RepositoryStatus,
};
use flotilla_schema::{Kind, KindCode, NewChange, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, StoreError};
use flotilla_store::{record_index_keys, ViewIndex};
use futures::stream::BoxStream;

use super::reads::created_at;
use super::resume::logged_since;
use super::watch::CommitsSeen;
use super::watch::SharedChanges;
use crate::stored::repository::{RepositoryRecord, VerificationProvenance};
use chrono::{DateTime, Utc};

const KIND: &str = "Repository";
const CODE: KindCode = <RepositoryRecord as Kind>::CODE;

/// How often a write that loses a race to another writer of its name reads and tries again.
const WRITE_ATTEMPTS: usize = 3;

/// The writer the plain-kind port records for this port's writes.
const WRITER: &str = "repository-records";

/// One home's record as the resident map holds it: the resource it reads as, or why it does not.
type Held = Result<Record<Repository>, String>;

/// Every home's records, by name and home.
type Records = BTreeMap<(String, String), Held>;

/// Every home's records, with the index keys each is held under.
#[derive(Default)]
struct Listing {
    records: Records,
    index: ViewIndex<(String, String)>,
}

impl Listing {
    fn insert(&mut self, key: (String, String), held: Held) {
        match &held {
            Ok(record) => match record_index_keys(record) {
                Ok(keys) => self.index.set(&key, keys),
                Err(error) => {
                    tracing::warn!(%error, name = %key.0, home = %key.1, "a repository record is not indexed");
                    self.index.remove(&key);
                }
            },
            Err(_) => self.index.remove(&key),
        }
        self.records.insert(key, held);
    }

    fn remove(&mut self, key: &(String, String)) {
        self.index.remove(key);
        self.records.remove(key);
    }
}

struct Resident {
    follower: Follower,
    listing: Listing,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `record` as the resource: its ops data as annotations, its observations as the status.
fn object(name: String, home: String, version: u64, created_at: DateTime<Utc>, record: &RepositoryRecord) -> Held {
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
    Ok(Record { spec: record.spec().clone(), status: Some(status), name, home, version, created_at, labels: BTreeMap::new(), annotations })
}

fn reload(read: &mut Read<'_>, listing: &mut Listing, name: &str, home: &str) -> Result<(), StoreError> {
    let key = (name.to_owned(), home.to_owned());
    match read.resolve(CODE, name, home)?.map(|id| read.load::<RepositoryRecord>(id)).transpose()?.flatten() {
        Some(loaded) => {
            let created_at = created_at(read, loaded.id)?;
            listing.insert(key, object(loaded.name, loaded.home, loaded.version, created_at, &loaded.value))
        }
        None => listing.remove(&key),
    };
    Ok(())
}

fn rescan(read: &mut Read<'_>, listing: &mut Listing, home: &str) -> Result<(), StoreError> {
    let gone: Vec<_> = listing.records.keys().filter(|(_, owner)| owner == home).cloned().collect();
    for key in &gone {
        listing.remove(key);
    }
    let mut after = None;
    loop {
        let page = read.scan::<RepositoryRecord>(home, after, 64)?;
        for id in page.ids {
            if let Some(loaded) = read.load::<RepositoryRecord>(id)? {
                let key = (loaded.name.clone(), home.to_owned());
                let created_at = created_at(read, id)?;
                listing.insert(key, object(loaded.name, loaded.home, loaded.version, created_at, &loaded.value));
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
    /// Whether the store has committed since the resident map was last brought up to it.
    commits: CommitsSeen,
    // One follower for every watcher of these records on this daemon.
    changes: SharedChanges,
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
        let opened = Self {
            changes: SharedChanges::new(Arc::clone(&service), [CODE], "repositories"),
            commits: CommitsSeen::follow(&service, [CODE]),
            service,
            home,
            log,
            resident: Mutex::new(Resident { follower: Follower::new([CODE]), listing: Listing::default() }),
            clock,
        };
        let homes = opened.service.homes();
        {
            let mut resident = locked(&opened.resident);
            let Resident { follower, listing } = &mut *resident;
            opened
                .service
                .with_store(|store| {
                    let mut read = store.read()?;
                    follower.start(&mut read, &homes)?;
                    for home in &homes {
                        rescan(&mut read, listing, home)?;
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
            let Resident { follower, listing } = &mut *resident;
            self.service
                .with_store(|store| {
                    let mut read = store.read()?;
                    for followed in follower.pass(&mut read, &homes)? {
                        match followed {
                            Followed::Changed(identity) if identity.kind == CODE => {
                                reload(&mut read, listing, &identity.name, &identity.home)?
                            }
                            Followed::Changed(_) => {}
                            Followed::Rescan(home) => rescan(&mut read, listing, &home)?,
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

    /// Writes this home's record `name` as `edit` makes it from the current one (or none),
    /// re-reading and trying again when another writer of the name wins the race; refused as
    /// stale instead when an `expected` version is given and the record has moved from it.
    fn write(
        &self,
        name: &str,
        expected: Option<u64>,
        edit: impl Fn(Option<RepositoryRecord>) -> Result<RepositoryRecord, String>,
    ) -> Result<Record<Repository>, WriteError> {
        for _ in 0..WRITE_ATTEMPTS {
            let current = self.log.get::<RepositoryRecord>(name, &self.home).map_err(|error| error.to_string())?;
            let version = current.as_ref().map(|current| current.version);
            WriteError::unless_at(expected, version)?;
            let value = edit(current.as_ref().map(|current| current.spec.clone()))?;
            let written = match (current, version) {
                (Some(current), Some(version)) if current.spec == value => Ok(version),
                (Some(_), Some(version)) => self.log.edit(name, version, &value, &PendingChange::new(NewChange::new(WRITER, self.now()))),
                _ => self.log.create(name, &value, &PendingChange::new(NewChange::new(WRITER, self.now()))),
            };
            match written {
                Ok(version) => {
                    let created_at =
                        self.log.created::<RepositoryRecord>(name, &self.home).map_err(|error| error.to_string())?.to_datetime();
                    return Ok(object(name.to_owned(), self.home.clone(), version, created_at, &value)?);
                }
                Err(PlainError::Conflict { .. } | PlainError::Exists { .. } | PlainError::Missing { .. }) => continue,
                Err(error) => return Err(format!("write {KIND} {name}: {error}").into()),
            }
        }
        Err(format!("write {KIND} {name}: lost {WRITE_ATTEMPTS} races with concurrent writers").into())
    }
}

impl flotilla_store::EnginePort for EngineRepositories {
    fn install(self: Arc<Self>, backend: &flotilla_store::ResourceBackend) {
        backend.install_engine_kind::<Repository>(self);
    }
}

#[async_trait::async_trait]
impl KindRecords<Repository> for EngineRepositories {
    fn codes(&self) -> Vec<KindCode> {
        vec![CODE]
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

    async fn get(&self, name: &str) -> Result<Option<Record<Repository>>, String> {
        let resident = self.refresh()?;
        resolve(&resident.listing.records, &self.home, name).transpose()
    }

    async fn list(&self) -> Result<Vec<Record<Repository>>, String> {
        let resident = self.refresh()?;
        let mut names: Vec<&String> = resident.listing.records.keys().map(|(name, _)| name).collect();
        // The map orders by name, then home, so one name's homes are adjacent.
        names.dedup();
        Ok(names
            .into_iter()
            .filter_map(|name| resolve(&resident.listing.records, &self.home, name))
            .filter_map(|held| held.map_err(|error| tracing::warn!(%error, "skipping a repository record that does not encode")).ok())
            .collect())
    }

    async fn sources(&self) -> Result<Vec<Record<Repository>>, String> {
        let resident = self.refresh()?;
        Ok(resident
            .listing
            .records
            .values()
            .filter_map(|held| {
                held.clone().map_err(|error| tracing::warn!(%error, "skipping a repository record that does not encode")).ok()
            })
            .collect())
    }

    async fn positioned(&self) -> Result<(Vec<Record<Repository>>, LogPosition), String> {
        let resident = self.refresh()?;
        let records = resident
            .listing
            .records
            .values()
            .filter_map(|held| {
                held.clone().map_err(|error| tracing::warn!(%error, "skipping a repository record that does not encode")).ok()
            })
            .collect();
        Ok((records, LogPosition::logged(resident.follower.cursors().clone())))
    }

    async fn position(&self) -> Result<LogPosition, String> {
        Ok(LogPosition::logged(self.refresh()?.follower.cursors().clone()))
    }

    async fn changed_since(&self, position: &LogPosition) -> Result<Since, String> {
        logged_since(&self.service, [CODE], position)
    }

    /// Every home's record `name`, read from the resident map by name.
    async fn copies(&self, name: &str) -> Result<Vec<Record<Repository>>, String> {
        let resident = self.refresh()?;
        Ok(resident
            .listing
            .records
            .range((name.to_owned(), String::new())..)
            .take_while(|((held, _), _)| held == name)
            .filter_map(|(_, held)| {
                held.clone().map_err(|error| tracing::warn!(%error, "skipping a repository record that does not encode")).ok()
            })
            .collect())
    }

    /// The records `cover` selects, from the resident map's index: this home's alone when `local`.
    async fn covered(&self, cover: &IndexCover, local: bool) -> Option<Result<Vec<Record<Repository>>, String>> {
        let resident = match self.refresh() {
            Ok(resident) => resident,
            Err(error) => return Some(Err(error)),
        };
        let listing = &resident.listing;
        Some(Ok(listing
            .index
            .covered(cover)
            .into_iter()
            .filter(|(_, home)| !local || *home == self.home)
            .filter_map(|key| listing.records.get(&key)?.clone().ok())
            .collect()))
    }

    async fn apply(
        &self,
        name: &str,
        version: Option<u64>,
        _labels: &BTreeMap<String, String>,
        annotations: &BTreeMap<String, String>,
        spec: &RepositorySpec,
    ) -> Result<Record<Repository>, WriteError> {
        spec.verify_key(&RepositoryKey(name.to_owned()))?;
        let verification_provenance = match annotations.get(VERIFICATION_PROVENANCE_ANNOTATION) {
            Some(json) => serde_json::from_str::<Vec<VerificationProvenance>>(json)
                .map_err(|error| format!("`{VERIFICATION_PROVENANCE_ANNOTATION}` does not decode: {error}"))?,
            None => Vec::new(),
        };
        self.write(name, version, |current| {
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

    async fn set_status(&self, name: &str, version: u64, status: &RepositoryStatus) -> Result<Record<Repository>, WriteError> {
        self.write(name, Some(version), |current| {
            let mut record = current.ok_or_else(|| format!("{KIND} {name} is not held here"))?;
            // Checkouts by host and diagnostics have no reader and are not kept (#3824 layer 3a).
            record.default_branch.clone_from(&status.default_branch);
            record.default_branch_observations.clone_from(&status.default_branch_observations);
            Ok(record)
        })
    }

    async fn delete(&self, name: &str) -> Result<(), String> {
        let Some(current) = self.log.get::<RepositoryRecord>(name, &self.home).map_err(|error| error.to_string())? else { return Ok(()) };
        match self.log.delete::<RepositoryRecord>(name, current.version, &PendingChange::new(NewChange::new(WRITER, self.now()))) {
            Ok(()) | Err(PlainError::Missing { .. }) => Ok(()),
            Err(error) => Err(format!("delete {KIND} {name}: {error}")),
        }
    }

    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        self.changes.subscribe().map_err(|error| error.to_string())
    }
}
