//! Project on the engine, behind `[primitives]` (#3824 layer 3b).
//!
//! [`EngineProjects`] is the [`EngineRecords`] port of Project over the decided store. Each home
//! writes the Projects it declares through the plain-kind port, and project operations' record of
//! each beside it; every home's are read from a resident map kept current by following the log.
//!
//! A Project reads as the old object:
//! - its origin as the stamps its writers wrote, rebuilt from its last change as on every
//!   definition kind ([`provenance`](super::provenance)): its row holds only its own fields (#4024);
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
    plain::{PendingChange, PlainError, PlainLog},
};
use flotilla_resources::{
    definitions::{origin, stamp, DefinitionOrigin},
    engine_records::{EngineObject, EngineRecords},
    DeclarationRefusedCondition, DispatchQueueAttention, DispatchQueueEntry, OperationalEntriesCondition, ProjectSpec, ProjectStatus,
};
use flotilla_schema::{Identity, Kind, KindCode, NewChange, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, ResourceId, StoreError};
use futures::stream::BoxStream;
use serde_json::Value;

use super::provenance;
use crate::stored::manifest_root::ManifestRoot;
use crate::stored::project::{ProjectDefinition, ProjectOpsRecord};

const KIND: &str = "Project";
const PROJECT: KindCode = <ProjectDefinition as Kind>::CODE;
const OPS: KindCode = <ProjectOpsRecord as Kind>::CODE;
/// The ManifestRoot's code: a root's baselines hold the paths its Projects read with, so a root's
/// change re-reads its home's Projects.
const ROOT: KindCode = <flotilla_state_machines::Resource<ManifestRoot> as Kind>::CODE;

/// How often a change stream looks for new entries in the log, as the account port's does.
const POLL: Duration = Duration::from_millis(500);

/// How often a write that loses a race to another writer of its name reads and tries again.
const WRITE_ATTEMPTS: usize = 3;

/// The writer of the project operations' records, and of a delete.
const WRITER: &str = "project-records";

/// One home's records of one Project name, with their versions; the definition with its origin,
/// rebuilt from its last change.
#[derive(Clone, Default)]
struct Held {
    project: Option<(u64, ProjectDefinition, DefinitionOrigin)>,
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
    at.to_datetime().unwrap_or_else(|| {
        tracing::warn!(%at, "a project operations time is outside the representable range; reading it as the epoch");
        Default::default()
    })
}

/// `held` as the old object, with `judged` (this home's judgement) when the record is this home's.
fn object(name: &str, home: &str, held: &Held, judged: Option<&Judged>) -> Option<Result<EngineObject, String>> {
    let (version, project, origin) = held.project.as_ref()?;
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
    let (labels, annotations) = stamp(origin);
    let version = composed_version(*version, held.ops.as_ref().map(|(version, _)| *version), judged.map_or(0, |judged| judged.revision));
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

/// The version a Project reads at: its records' latest log version, above the judgement's
/// revision. Log versions are this home's write sequence, so they only rise, across a delete and
/// a create of the same name too; a judgement change raises the low part under an unchanged log.
/// Two accepted gaps, both in the low part: a restart forgets the judgement, and its revision wraps
/// after 2^24 changes under one log version; either may repeat a version read before.
fn composed_version(project: u64, ops: Option<u64>, revision: u64) -> u64 {
    let logged = project.max(ops.unwrap_or(0));
    debug_assert!(logged < 1 << (u64::BITS - JUDGEMENT_BITS), "log version {logged} leaves no room for the judgement's revision");
    (logged << JUDGEMENT_BITS) | (revision & ((1 << JUDGEMENT_BITS) - 1))
}

/// The low bits of a Project's version that count judgement changes.
const JUDGEMENT_BITS: u32 = 24;

/// Project `id`, with its origin as its last change says it.
fn load_project(read: &mut Read<'_>, id: ResourceId) -> Result<Option<(String, u64, ProjectDefinition, DefinitionOrigin)>, StoreError> {
    let Some(loaded) = read.load::<ProjectDefinition>(id)? else { return Ok(None) };
    let change = provenance::last_change(read, id)?;
    let origin = provenance::origin_of(read, KIND, &loaded.name, change.as_ref(), None)?;
    Ok(Some((loaded.name, loaded.version, loaded.value, origin)))
}

fn reload(read: &mut Read<'_>, records: &mut Records, identity: &Identity) -> Result<(), StoreError> {
    let key = (identity.name.clone(), identity.home.clone());
    let id = read.resolve(identity.kind, &identity.name, &identity.home)?;
    let held = records.entry(key.clone()).or_default();
    if identity.kind == PROJECT {
        held.project =
            id.map(|id| load_project(read, id)).transpose()?.flatten().map(|(_, version, value, origin)| (version, value, origin));
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
            if let Some((name, version, value, origin)) = load_project(read, id)? {
                records.entry((name, home.to_owned())).or_default().project = Some((version, value, origin));
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
    /// Held across each write, so this home's writes of one Project (its definition and its
    /// operations record together) never interleave. Taken before `resident` and `judged`.
    writing: Mutex<()>,
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
            resident: Mutex::new(Resident { follower: Follower::new([PROJECT, OPS, ROOT]), records: Records::new() }),
            judged: Mutex::default(),
            writing: Mutex::default(),
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
                            Followed::Changed(identity) if identity.kind == ROOT => rescan(&mut read, records, &identity.home)?,
                            Followed::Changed(_) => {}
                            Followed::Rescan(home) => rescan(&mut read, records, &home)?,
                        }
                    }
                    Ok(())
                })
                .map_err(|error| error.to_string())?;
        }
        // A judgement outlives no Project of this home: whatever removed it, a later one of the
        // name starts afresh (its log version is above anything the old one read at).
        locked(&self.judged)
            .retain(|name, _| resident.records.get(&(name.clone(), self.home.clone())).is_some_and(|held| held.project.is_some()));
        Ok(resident)
    }

    fn now(&self) -> Timestamp {
        Timestamp::from_datetime(&self.clock.now())
    }

    /// Every record of `name` as the old object, by home; this home's with its judgement.
    fn objects(&self, records: &Records, judged: &BTreeMap<String, Judged>, name: &str) -> Vec<Result<EngineObject, String>> {
        records
            .range((name.to_owned(), String::new())..)
            .take_while(|((held, _), _)| held == name)
            .filter_map(|((_, home), held)| object(name, home, held, (*home == self.home).then(|| judged.get(name)).flatten()))
            .collect()
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
    /// none leaves the record as it is. A record written for `origin` is its change's: unchanged
    /// but declared for a new reason, it is written again under that change (#4024).
    fn write<K: Kind + Clone + PartialEq>(
        &self,
        name: &str,
        origin: Option<&DefinitionOrigin>,
        edit: impl Fn(Option<&K>) -> Result<Option<K>, String>,
    ) -> Result<(), String> {
        for _ in 0..WRITE_ATTEMPTS {
            let change = match origin {
                Some(origin) => PendingChange::joining(provenance::change_of(origin, &self.home, self.now())),
                None => PendingChange::new(NewChange::new(WRITER, self.now())),
            };
            let current = self.log.get::<K>(name, &self.home).map_err(|error| error.to_string())?;
            let Some(value) = edit(current.as_ref().map(|current| &current.spec))? else { return Ok(()) };
            let written = match current {
                Some(current) if current.spec == value => {
                    if origin.is_none() {
                        return Ok(());
                    }
                    let last = self.log.last_change::<K>(name, &self.home).map_err(|error| error.to_string())?;
                    if last.is_some_and(|last| last.writer == change.change().writer && last.items == change.change().items) {
                        return Ok(());
                    }
                    self.log.touch::<K>(name, current.version, &change)
                }
                Some(current) => self.log.edit(name, current.version, &value, &change),
                None => self.log.create(name, &value, &change),
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
        match self.log.delete::<K>(name, current.version, &PendingChange::new(NewChange::new(WRITER, self.now()))) {
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

/// One name's records as several homes' resolve at `home`: its own, else the lexically first
/// home's (the order `objects` gives).
fn prefer(mut objects: Vec<Result<EngineObject, String>>, home: &str) -> Option<Result<EngineObject, String>> {
    match objects.iter().position(|object| object.as_ref().is_ok_and(|object| object.home == home)) {
        Some(index) => Some(objects.swap_remove(index)),
        None => objects.into_iter().next(),
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
        let judged = locked(&self.judged);
        prefer(self.objects(&resident.records, &judged, name), &self.home).transpose()
    }

    async fn list(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        refuse_kind(kind)?;
        let resident = self.refresh()?;
        let mut names: Vec<&String> = resident.records.keys().map(|(name, _)| name).collect();
        // The map orders by name, then home, so one name's homes are adjacent.
        names.dedup();
        let judged = locked(&self.judged);
        Ok(names
            .into_iter()
            .filter_map(|name| prefer(self.objects(&resident.records, &judged, name), &self.home))
            .filter_map(skip_unencoded)
            .collect())
    }

    async fn sources(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        refuse_kind(kind)?;
        let resident = self.refresh()?;
        let mut names: Vec<&String> = resident.records.keys().map(|(name, _)| name).collect();
        names.dedup();
        let judged = locked(&self.judged);
        Ok(names.into_iter().flat_map(|name| self.objects(&resident.records, &judged, name)).filter_map(skip_unencoded).collect())
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
        let origin = origin(labels, annotations)?;
        let value = ProjectDefinition::new(&spec).map_err(|error| format!("{KIND} {name}: {error}"))?;
        let _writing = locked(&self.writing);
        if self.log.get::<ProjectDefinition>(name, &self.home).map_err(|error| error.to_string())?.is_none() {
            // A new Project starts with no conditions: an operations record left by a delete that
            // stopped part-way belongs to the one before. `writing` keeps any other write of the
            // name out until the definition below is written.
            self.remove::<ProjectOpsRecord>(name)?;
        }
        self.write::<ProjectDefinition>(name, Some(&origin), |_| Ok(Some(value.clone())))?;
        self.current(name)
    }

    async fn set_status(&self, kind: &str, name: &str, status: Value) -> Result<EngineObject, String> {
        refuse_kind(kind)?;
        let status: ProjectStatus = serde_json::from_value(status).map_err(|error| format!("{KIND} status: {error}"))?;
        let _writing = locked(&self.writing);
        self.current(name)?;
        // Project operations' conditions are their record; once written it stays, emptied, until
        // the Project goes, so its version only moves forward.
        let ops = ProjectOpsRecord::from_status(&status);
        self.write::<ProjectOpsRecord>(name, None, |current| {
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
        let _writing = locked(&self.writing);
        // The operations record first: a delete stopped between the two leaves a Project with no
        // conditions, never conditions with no Project.
        self.remove::<ProjectOpsRecord>(name)?;
        self.remove::<ProjectDefinition>(name)?;
        if locked(&self.judged).remove(name).is_some() {
            self.judged_changes.send_modify(|tick| *tick += 1);
        }
        Ok(())
    }

    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let service = Arc::clone(&self.service);
        let mut follower = Follower::new([PROJECT, OPS, ROOT]);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn held(home: &str) -> Result<EngineObject, String> {
        Ok(EngineObject {
            name: "widgets".into(),
            home: home.into(),
            version: 1,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            spec: Value::Null,
            status: None,
        })
    }

    // Behaviour (#3824 layer 3b, as the import placed records): a name several homes hold reads as
    // this home's record, else the lexically first home's.
    #[test]
    fn a_name_several_homes_hold_reads_as_this_homes_else_the_first() {
        let homes = || ["feta", "kiwi", "udder"].map(held).to_vec();
        let home_of = |at: &str| prefer(homes(), at).expect("held").expect("encodes").home;
        assert_eq!(home_of("kiwi"), "kiwi");
        assert_eq!(home_of("b5"), "feta");
        assert!(prefer(Vec::new(), "kiwi").is_none());
    }
}
