//! Project on the engine, behind `[primitives]` (#3824 layer 3b).
//!
//! [`EngineProjects`] is the [`EngineRecords`] port of Project over the decided store. Each home
//! writes the Projects it declares through the plain-kind port, and project operations' record of
//! each beside it; every home's are read from a resident map kept current by following the log.
//!
//! A Project reads as the old object:
//! - its origin as the stamps its writers wrote, as on every definition kind;
//! - its status from two sources. Project operations' conditions come from the [`ProjectOpsRecord`]
//!   at the Project's home. The dispatch queue, its error and its attention are the dispatch
//!   reconciler's judgement: held in memory at the home that computes it, served on that home's
//!   reads, never stored. A restart recomputes it.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use flotilla_engine_store::{
    follow::{Followed, Follower},
    plain::{PlainError, PlainLog},
};
use flotilla_resources::{
    definitions::{origin, stamp},
    engine_records::{EngineObject, EngineRecords},
    DeclarationRefusedCondition, DispatchQueueAttention, DispatchQueueEntry, OperationalEntriesCondition, ProjectSpec, ProjectStatus,
};
use flotilla_schema::{Identity, Kind, KindCode, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, StoreError};
use futures::stream::BoxStream;
use serde_json::Value;

use crate::stored::project::{ProjectDefinition, ProjectOpsRecord};

const KIND: &str = "Project";
const PROJECT: KindCode = <ProjectDefinition as Kind>::CODE;
const OPS: KindCode = <ProjectOpsRecord as Kind>::CODE;

/// How often a change stream looks for new entries in the log, as the account port's does.
const POLL: Duration = Duration::from_millis(500);

/// How often a write that loses a race to another writer of its name reads and tries again.
const WRITE_ATTEMPTS: usize = 3;

/// The writer the plain-kind port records for this port's writes.
const WRITER: &str = "project-records";

/// One home's records of one Project name, with their versions.
#[derive(Clone, Default)]
struct Held {
    project: Option<(u64, ProjectDefinition)>,
    ops: Option<(u64, ProjectOpsRecord)>,
}

/// Every home's records, by name and home.
type Records = BTreeMap<(String, String), Held>;

/// The dispatch reconciler's judgement of one Project at this home, with a revision that moves on
/// each change, so a Project's version moves with it.
#[derive(Clone, Default, PartialEq)]
struct Queue {
    entries: Vec<DispatchQueueEntry>,
    error: Option<String>,
    attention: Option<DispatchQueueAttention>,
}

#[derive(Default)]
struct Judged {
    queue: Queue,
    revision: u64,
}

struct Resident {
    follower: Follower,
    records: Records,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn datetime(at: Timestamp) -> chrono::DateTime<chrono::Utc> {
    at.to_datetime().unwrap_or_default()
}

/// `held` as the old object, with `judged` (this home's judgement) when the record is this home's.
fn object(name: &str, home: &str, held: &Held, judged: Option<&Judged>) -> Option<Result<EngineObject, String>> {
    let (version, project) = held.project.as_ref()?;
    let encode = |error: serde_json::Error| format!("{KIND} {name} at {home}: {error}");
    let ops = held.ops.as_ref().map(|(_, ops)| ops);
    let queue = judged.map(|judged| judged.queue.clone()).unwrap_or_default();
    let status = ProjectStatus {
        declaration_refused: ops.and_then(|ops| ops.refused.as_ref()).map(|refused| DeclarationRefusedCondition {
            entry_path: refused.entry_path.clone(),
            message: refused.message.clone(),
            since: datetime(refused.since),
            observed_at: datetime(refused.observed_at),
        }),
        operational_entries: ops
            .and_then(|ops| ops.entries.as_ref())
            .map(|entries| OperationalEntriesCondition { ready: entries.ready, message: entries.message.clone() }),
        dispatch_queue: queue.entries,
        dispatch_queue_error: queue.error,
        dispatch_queue_attention: queue.attention,
    };
    let (labels, annotations) = stamp(&project.origin);
    // Each part's version only moves forward while this home runs, so their sum moves with any.
    let version = version + held.ops.as_ref().map_or(0, |(version, _)| *version) + judged.map_or(0, |judged| judged.revision);
    Some((|| {
        Ok(EngineObject {
            name: name.to_owned(),
            home: home.to_owned(),
            version,
            labels,
            annotations,
            spec: serde_json::to_value(project.spec()).map_err(encode)?,
            status: Some(serde_json::to_value(status).map_err(encode)?),
        })
    })())
}

fn reload(read: &mut Read<'_>, records: &mut Records, identity: &Identity) -> Result<(), StoreError> {
    let key = (identity.name.clone(), identity.home.clone());
    let id = read.resolve(identity.kind, &identity.name, &identity.home)?;
    let held = records.entry(key.clone()).or_default();
    if identity.kind == PROJECT {
        held.project = id.map(|id| read.load::<ProjectDefinition>(id)).transpose()?.flatten().map(|loaded| (loaded.version, loaded.value));
    } else if identity.kind == OPS {
        held.ops = id.map(|id| read.load::<ProjectOpsRecord>(id)).transpose()?.flatten().map(|loaded| (loaded.version, loaded.value));
    }
    if held.project.is_none() && held.ops.is_none() {
        records.remove(&key);
    }
    Ok(())
}

fn rescan(read: &mut Read<'_>, records: &mut Records, home: &str) -> Result<(), StoreError> {
    records.retain(|(_, owner), _| owner != home);
    let mut after = None;
    loop {
        let page = read.scan::<ProjectDefinition>(home, after, 64)?;
        for id in page.ids {
            if let Some(loaded) = read.load::<ProjectDefinition>(id)? {
                records.entry((loaded.name, home.to_owned())).or_default().project = Some((loaded.version, loaded.value));
            }
        }
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    loop {
        let page = read.scan::<ProjectOpsRecord>(home, after, 64)?;
        for id in page.ids {
            if let Some(loaded) = read.load::<ProjectOpsRecord>(id)? {
                records.entry((loaded.name, home.to_owned())).or_default().ops = Some((loaded.version, loaded.value));
            }
        }
        after = page.next;
        if after.is_none() {
            return Ok(());
        }
    }
}

/// The Project records of the decided store at one home.
pub struct EngineProjects {
    service: Arc<Service>,
    home: String,
    log: PlainLog,
    resident: Mutex<Resident>,
    /// This home's dispatch judgements, by Project name.
    judged: Mutex<BTreeMap<String, Judged>>,
    /// Ticks on each change of a judgement, which the log does not carry.
    judged_changes: tokio::sync::watch::Sender<u64>,
    clock: Arc<dyn flotilla_resources::Clock>,
}

impl std::fmt::Debug for EngineProjects {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("EngineProjects").field("home", &self.home).finish_non_exhaustive()
    }
}

impl EngineProjects {
    /// Opens this home's Project writer over `service`, and reads every home's records.
    pub fn open(service: Arc<Service>, clock: Arc<dyn flotilla_resources::Clock>) -> Result<Self, String> {
        let log = PlainLog::new(service.store());
        let home = log.home().to_owned();
        let opened = Self {
            service,
            home,
            log,
            resident: Mutex::new(Resident { follower: Follower::new([PROJECT, OPS]), records: Records::new() }),
            judged: Mutex::default(),
            judged_changes: tokio::sync::watch::Sender::new(0),
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
                            Followed::Changed(identity) if identity.kind == PROJECT || identity.kind == OPS => {
                                reload(&mut read, records, &identity)?
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

    /// Every record of `name` as the old object, by home; this home's with its judgement.
    fn objects(&self, records: &Records, name: &str) -> Vec<Result<EngineObject, String>> {
        let judged = locked(&self.judged);
        records
            .range((name.to_owned(), String::new())..)
            .take_while(|((held, _), _)| held == name)
            .filter_map(|((_, home), held)| object(name, home, held, (*home == self.home).then(|| judged.get(name)).flatten()))
            .collect()
    }

    /// `name` as several homes' resolve: this home's record, else the lexically first home's.
    fn resolve(&self, records: &Records, name: &str) -> Option<Result<EngineObject, String>> {
        let mut objects = self.objects(records, name);
        let mine = objects.iter().position(|object| object.as_ref().is_ok_and(|object| object.home == self.home));
        match mine {
            Some(index) => Some(objects.swap_remove(index)),
            None => objects.into_iter().next(),
        }
    }

    /// This home's current object `name`, read again from the store.
    fn current(&self, name: &str) -> Result<EngineObject, String> {
        let resident = self.refresh()?;
        let records = &resident.records;
        let held = records.get(&(name.to_owned(), self.home.clone())).ok_or_else(|| format!("{KIND} {name} is not held here"))?;
        let judged = locked(&self.judged);
        object(name, &self.home, held, judged.get(name)).ok_or_else(|| format!("{KIND} {name} is not held here"))?
    }

    /// Writes this home's record `name` of `K` as `edit` makes it from the current one (or none),
    /// re-reading and trying again when another writer of the name wins the race. `edit` returning
    /// none leaves the record as it is.
    fn write<K: Kind + Clone + PartialEq>(&self, name: &str, edit: impl Fn(Option<&K>) -> Result<Option<K>, String>) -> Result<(), String> {
        for _ in 0..WRITE_ATTEMPTS {
            let current = self.log.get::<K>(name, &self.home).map_err(|error| error.to_string())?;
            let Some(value) = edit(current.as_ref().map(|current| &current.spec))? else { return Ok(()) };
            let written = match current {
                Some(current) if current.spec == value => return Ok(()),
                Some(current) => self.log.edit(name, current.version, &value, WRITER, self.now()),
                None => self.log.create(name, &value, WRITER, self.now()),
            };
            match written {
                Ok(_) => return Ok(()),
                Err(PlainError::Conflict { .. } | PlainError::Exists { .. } | PlainError::Missing { .. }) => continue,
                Err(error) => return Err(format!("write {} {name}: {error}", K::KIND)),
            }
        }
        Err(format!("write {} {name}: lost {WRITE_ATTEMPTS} races with concurrent writers", K::KIND))
    }

    fn remove<K: Kind>(&self, name: &str) -> Result<(), String> {
        let Some(current) = self.log.get::<K>(name, &self.home).map_err(|error| error.to_string())? else { return Ok(()) };
        match self.log.delete::<K>(name, current.version, WRITER, self.now()) {
            Ok(()) | Err(PlainError::Missing { .. }) => Ok(()),
            Err(error) => Err(format!("delete {} {name}: {error}", K::KIND)),
        }
    }

    /// Replaces this home's judgement of `name`; a change moves its revision and ticks watchers.
    fn judge(&self, name: &str, queue: Queue) {
        let mut judged = locked(&self.judged);
        let held = judged.entry(name.to_owned()).or_default();
        if held.queue != queue {
            held.queue = queue;
            held.revision += 1;
            self.judged_changes.send_modify(|tick| *tick += 1);
        }
    }
}

fn refuse_kind(kind: &str) -> Result<(), String> {
    if kind == KIND {
        Ok(())
    } else {
        Err(format!("{kind} is not served by the project records"))
    }
}

fn skip_unencoded(held: Result<EngineObject, String>) -> Option<EngineObject> {
    held.map_err(|error| tracing::warn!(%error, "skipping a project record that does not encode")).ok()
}

#[async_trait::async_trait]
impl EngineRecords for EngineProjects {
    fn kinds(&self) -> &'static [&'static str] {
        &[KIND]
    }

    fn home(&self) -> String {
        self.home.clone()
    }

    async fn get(&self, kind: &str, name: &str) -> Result<Option<EngineObject>, String> {
        refuse_kind(kind)?;
        let resident = self.refresh()?;
        self.resolve(&resident.records, name).transpose()
    }

    async fn list(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        refuse_kind(kind)?;
        let resident = self.refresh()?;
        let mut names: Vec<&String> = resident.records.keys().map(|(name, _)| name).collect();
        // The map orders by name, then home, so one name's homes are adjacent.
        names.dedup();
        Ok(names.into_iter().filter_map(|name| self.resolve(&resident.records, name)).filter_map(skip_unencoded).collect())
    }

    async fn sources(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        refuse_kind(kind)?;
        let resident = self.refresh()?;
        let mut names: Vec<&String> = resident.records.keys().map(|(name, _)| name).collect();
        names.dedup();
        Ok(names.into_iter().flat_map(|name| self.objects(&resident.records, name)).filter_map(skip_unencoded).collect())
    }

    async fn apply(
        &self,
        kind: &str,
        name: &str,
        labels: &BTreeMap<String, String>,
        annotations: &BTreeMap<String, String>,
        spec: Value,
    ) -> Result<EngineObject, String> {
        refuse_kind(kind)?;
        let spec: ProjectSpec = serde_json::from_value(spec).map_err(|error| format!("{KIND} spec: {error}"))?;
        let value = ProjectDefinition::new(origin(labels, annotations)?, &spec).map_err(|error| format!("{KIND} {name}: {error}"))?;
        self.write::<ProjectDefinition>(name, |_| Ok(Some(value.clone())))?;
        self.current(name)
    }

    async fn set_status(&self, kind: &str, name: &str, status: Value) -> Result<EngineObject, String> {
        refuse_kind(kind)?;
        let status: ProjectStatus = serde_json::from_value(status).map_err(|error| format!("{KIND} status: {error}"))?;
        self.current(name)?;
        // Project operations' conditions are their record; once written it stays, emptied, until
        // the Project goes, so its version only moves forward.
        let ops = ProjectOpsRecord::from_status(&status);
        self.write::<ProjectOpsRecord>(name, |current| {
            Ok(match (&ops, current) {
                (Some(ops), _) => Some(ops.clone()),
                (None, Some(_)) => Some(ProjectOpsRecord { refused: None, entries: None }),
                (None, None) => None,
            })
        })?;
        self.judge(
            name,
            Queue { entries: status.dispatch_queue, error: status.dispatch_queue_error, attention: status.dispatch_queue_attention },
        );
        self.current(name)
    }

    async fn delete(&self, kind: &str, name: &str) -> Result<(), String> {
        refuse_kind(kind)?;
        self.remove::<ProjectDefinition>(name)?;
        self.remove::<ProjectOpsRecord>(name)?;
        if locked(&self.judged).remove(name).is_some() {
            self.judged_changes.send_modify(|tick| *tick += 1);
        }
        Ok(())
    }

    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let service = Arc::clone(&self.service);
        let mut follower = Follower::new([PROJECT, OPS]);
        let homes = service.homes();
        service.with_store(|store| follower.start(&mut store.read()?, &homes)).map_err(|error| error.to_string())?;
        let mut judged = self.judged_changes.subscribe();
        judged.mark_unchanged();
        let stream = futures::stream::unfold((service, follower, judged), |(service, mut follower, mut judged)| async move {
            loop {
                tokio::select! {
                    changed = judged.changed() => {
                        // The port is gone once its sender is; the log alone ticks from then on.
                        if changed.is_ok() {
                            return Some(((), (service, follower, judged)));
                        }
                        tokio::time::sleep(POLL).await;
                    }
                    () = tokio::time::sleep(POLL) => {}
                }
                let homes = service.homes();
                match service.with_store(|store| Ok(!follower.pass(&mut store.read()?, &homes)?.is_empty())) {
                    Ok(true) => return Some(((), (service, follower, judged))),
                    Ok(false) => {}
                    Err(error) => tracing::warn!(%error, "project change stream could not read the log; retrying"),
                }
            }
        });
        Ok(Box::pin(stream))
    }
}
