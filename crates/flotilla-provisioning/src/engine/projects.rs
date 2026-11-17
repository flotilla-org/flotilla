//! Project on the engine, behind `[primitives]` (#3824 layer 3b).
//!
//! [`EngineProjects`] is the [`KindRecords`] port of Project over the decided store. Each home
//! writes the Projects it declares through the plain-kind port, and project operations' record of
//! each beside it; every home's are read from a resident map kept current by following the log.
//!
//! A Project reads as the resource:
//! - its origin as the stamps its writers wrote, rebuilt from its last change as on every
//!   definition kind ([`provenance`](super::provenance)): its row holds only its own fields (#4024);
//! - its status from two sources. Project operations' conditions come from the [`ProjectOpsRecord`]
//!   at the Project's home. The dispatch queue, its error and its attention are the dispatch
//!   reconciler's judgement: held in memory at the home that computes it, served on that home's
//!   reads, never stored. A restart recomputes it.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use flotilla_engine_store::{
    follow::{Followed, Follower},
    plain::{PendingChange, PlainError, PlainLog},
};
use flotilla_resources::{
    definitions::{origin, stamp, DefinitionOrigin},
    engine_records::{CommitMark, KindRecords, LogPosition, Record, Since, StatusWriteError, Ticks, UnloggedJournal},
    DeclarationRefusedCondition, DispatchQueueAttention, DispatchQueueEntry, OperationalEntriesCondition, Project, ProjectSpec,
    ProjectStatus,
};
use flotilla_schema::{Identity, Kind, KindCode, NewChange, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, ResourceId, StoreError};
use futures::stream::BoxStream;

use super::provenance;
use super::reads::created_at;
use super::resume::followed_since;
use super::watch::CommitsSeen;
use super::watch::SharedChanges;
use crate::stored::manifest_root::ManifestRoot;
use crate::stored::project::{ProjectDefinition, ProjectOpsRecord};

const KIND: &str = "Project";
const PROJECT: KindCode = <ProjectDefinition as Kind>::CODE;
const OPS: KindCode = <ProjectOpsRecord as Kind>::CODE;
/// The ManifestRoot's code: a root's baselines hold the paths its Projects read with, so a root's
/// change re-reads its home's Projects.
const ROOT: KindCode = <flotilla_state_machines::Resource<ManifestRoot> as Kind>::CODE;

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
    /// When the Project's definition was created; none while none is held.
    created: Option<chrono::DateTime<chrono::Utc>>,
}

/// Every home's records, by name and home.
type Records = BTreeMap<(String, String), Held>;

/// One home's Project as the resource it reads as, or why it does not.
type Object = Result<Record<Project>, String>;

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

/// `held` as the resource, with `judged` (this home's judgement) when the record is this home's.
fn object(name: &str, home: &str, held: &Held, judged: Option<&Judged>) -> Option<Object> {
    let (version, project, origin) = held.project.as_ref()?;
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
    let Some(created_at) = held.created else { return Some(Err(format!("{KIND} {name} at {home} has no creation time"))) };
    Some(Ok(Record {
        name: name.to_owned(),
        home: home.to_owned(),
        version,
        created_at,
        labels,
        annotations,
        spec: project.spec().clone(),
        status: Some(status),
    }))
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
fn load_project(
    read: &mut Read<'_>,
    roots: &mut provenance::Roots,
    id: ResourceId,
) -> Result<Option<(String, u64, ProjectDefinition, DefinitionOrigin)>, StoreError> {
    let Some(loaded) = read.load::<ProjectDefinition>(id)? else { return Ok(None) };
    let change = read.last_change(id)?;
    let origin = provenance::origin_of(read, roots, KIND, &loaded.name, change.as_ref(), None)?;
    Ok(Some((loaded.name, loaded.version, loaded.value, origin)))
}

fn reload(read: &mut Read<'_>, records: &mut Records, identity: &Identity) -> Result<(), StoreError> {
    let key = (identity.name.clone(), identity.home.clone());
    let id = read.resolve(identity.kind, &identity.name, &identity.home)?;
    let held = records.entry(key.clone()).or_default();
    if identity.kind == PROJECT {
        held.project = id
            .map(|id| load_project(read, &mut provenance::Roots::default(), id))
            .transpose()?
            .flatten()
            .map(|(_, version, value, origin)| (version, value, origin));
        held.created = match (id, &held.project) {
            (Some(id), Some(_)) => Some(created_at(read, id)?),
            _ => None,
        };
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
    // One read of each root the home's Projects name, whatever their number.
    let mut roots = provenance::Roots::default();
    let mut after = None;
    loop {
        let page = read.scan::<ProjectDefinition>(home, after, 64)?;
        for id in page.ids {
            if let Some((name, version, value, origin)) = load_project(read, &mut roots, id)? {
                let created = created_at(read, id)?;
                let held = records.entry((name, home.to_owned())).or_default();
                (held.project, held.created) = (Some((version, value, origin)), Some(created));
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
    /// Whether the store has committed since the resident map was last brought up to it.
    commits: CommitsSeen,
    // One follower for every watcher of these records on this daemon.
    changes: SharedChanges,
    /// This home's dispatch judgements, by Project name.
    judged: Mutex<BTreeMap<String, Judged>>,
    /// Held across each write, so this home's writes of one Project (its definition and its
    /// operations record together) never interleave. Taken before `resident` and `judged`.
    writing: Mutex<()>,
    /// Ticks on each change of a judgement, which the log does not carry, with their count.
    judged_changes: tokio::sync::watch::Sender<u64>,
    /// The names each change of a judgement touched. Taken under `judged`.
    unlogged: Mutex<UnloggedJournal>,
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
            changes: SharedChanges::new(Arc::clone(&service), [PROJECT, OPS, ROOT], "projects"),
            commits: CommitsSeen::follow(&service, [PROJECT, OPS, ROOT]),
            service,
            home,
            log,
            resident: Mutex::new(Resident { follower: Follower::new([PROJECT, OPS, ROOT]), records: Records::new() }),
            judged: Mutex::default(),
            writing: Mutex::default(),
            judged_changes: tokio::sync::watch::Sender::new(0),
            unlogged: Mutex::default(),
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
                .map_err(|error| error.to_string())
                .inspect_err(|_| self.commits.give_back())?;
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

    /// Every record of `name` as the resource, by home; this home's with its judgement.
    fn objects(&self, records: &Records, judged: &BTreeMap<String, Judged>, name: &str) -> Vec<Object> {
        records
            .range((name.to_owned(), String::new())..)
            .take_while(|((held, _), _)| held == name)
            .filter_map(|((_, home), held)| object(name, home, held, (*home == self.home).then(|| judged.get(name)).flatten()))
            .collect()
    }

    /// This home's current Project `name`, read again from the store.
    fn current(&self, name: &str) -> Object {
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
                    // Read apart from the record: another write between the two moves its version,
                    // so the touch below is refused as a conflict and the loop reads again.
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
            self.judged_change(name);
        }
    }

    /// Counts a change of `name`'s judgement, under the judgements' lock, and ticks watchers.
    fn judged_change(&self, name: &str) {
        let count = locked(&self.unlogged).record([name.to_owned()]);
        self.judged_changes.send_replace(count);
    }

    /// A reading through `homes`' sequences and the judgements counted so far; taken under the
    /// judgements' lock, with the records it positions.
    fn positioned_at(&self, homes: BTreeMap<String, u64>) -> LogPosition {
        LogPosition { homes, unlogged: Some(locked(&self.unlogged).read()) }
    }
}

/// One name's records as several homes' resolve at `home`: its own, else the lexically first
/// home's (the order `objects` gives).
fn prefer(mut objects: Vec<Object>, home: &str) -> Option<Object> {
    match objects.iter().position(|object| object.as_ref().is_ok_and(|object| object.home == home)) {
        Some(index) => Some(objects.swap_remove(index)),
        None => objects.into_iter().next(),
    }
}

fn skip_unencoded(held: Object) -> Option<Record<Project>> {
    held.map_err(|error| tracing::warn!(%error, "skipping a project record that does not encode")).ok()
}

impl flotilla_store::EnginePort for EngineProjects {
    fn install(self: Arc<Self>, backend: &flotilla_store::ResourceBackend) {
        backend.install_engine_kind::<Project>(self);
    }
}

#[async_trait::async_trait]
impl KindRecords<Project> for EngineProjects {
    fn codes(&self) -> Vec<KindCode> {
        vec![PROJECT, OPS, ROOT]
    }

    /// From the shared follower of the log, and from the judge of dispatch.
    fn ticks(&self) -> Ticks {
        Ticks::Followed
    }

    /// A judgement ticks as it changes.
    fn commit_mark(&self) -> Option<CommitMark> {
        Some(super::commit_mark(&self.service, self.codes()))
    }

    /// This home's dispatch judgements.
    fn unlogged_changes(&self) -> Option<u64> {
        Some(locked(&self.unlogged).count())
    }

    fn home(&self) -> String {
        self.home.clone()
    }

    async fn get(&self, name: &str) -> Result<Option<Record<Project>>, String> {
        let resident = self.refresh()?;
        let judged = locked(&self.judged);
        prefer(self.objects(&resident.records, &judged, name), &self.home).transpose()
    }

    async fn list(&self) -> Result<Vec<Record<Project>>, String> {
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

    async fn sources(&self) -> Result<Vec<Record<Project>>, String> {
        let resident = self.refresh()?;
        let mut names: Vec<&String> = resident.records.keys().map(|(name, _)| name).collect();
        names.dedup();
        let judged = locked(&self.judged);
        Ok(names.into_iter().flat_map(|name| self.objects(&resident.records, &judged, name)).filter_map(skip_unencoded).collect())
    }

    async fn positioned(&self) -> Result<(Vec<Record<Project>>, LogPosition), String> {
        let resident = self.refresh()?;
        let mut names: Vec<&String> = resident.records.keys().map(|(name, _)| name).collect();
        names.dedup();
        let judged = locked(&self.judged);
        let records =
            names.into_iter().flat_map(|name| self.objects(&resident.records, &judged, name)).filter_map(skip_unencoded).collect();
        Ok((records, self.positioned_at(resident.follower.cursors().clone())))
    }

    async fn position(&self) -> Result<LogPosition, String> {
        let resident = self.refresh()?;
        let _judged = locked(&self.judged);
        Ok(self.positioned_at(resident.follower.cursors().clone()))
    }

    /// A root's change names every Project of its home: its baselines hold the paths they read with.
    async fn changed_since(&self, position: &LogPosition) -> Result<Since, String> {
        let (changed, through) = match followed_since(&self.service, [PROJECT, OPS, ROOT], position)? {
            Ok(followed) => followed,
            Err(reason) => return Ok(Since::Expired(reason)),
        };
        let resident = self.refresh()?;
        let _judged = locked(&self.judged);
        let mut names = match locked(&self.unlogged).since(position.unlogged.as_ref()) {
            Ok(names) => names,
            Err(reason) => return Ok(Since::Expired(reason)),
        };
        for identity in changed {
            if identity.kind == ROOT {
                names.extend(resident.records.keys().filter(|(_, home)| *home == identity.home).map(|(name, _)| name.clone()));
            } else {
                names.insert(identity.name);
            }
        }
        Ok(Since::Changed { names, through: self.positioned_at(through) })
    }

    /// Every home's record `name`, read from the resident map by name.
    async fn copies(&self, name: &str) -> Result<Vec<Record<Project>>, String> {
        let resident = self.refresh()?;
        let judged = locked(&self.judged);
        Ok(self.objects(&resident.records, &judged, name).into_iter().filter_map(skip_unencoded).collect())
    }

    async fn apply(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
        annotations: &BTreeMap<String, String>,
        spec: &ProjectSpec,
    ) -> Object {
        let origin = provenance::resolve_stamped_root(&self.service, &self.home, origin(labels, annotations)?)?;
        let value = ProjectDefinition::new(spec).map_err(|error| format!("{KIND} {name}: {error}"))?;
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

    async fn set_status(&self, name: &str, version: u64, status: &ProjectStatus) -> Result<Record<Project>, StatusWriteError> {
        let _writing = locked(&self.writing);
        if self.current(name)?.version != version {
            return Err(StatusWriteError::Stale);
        }
        // Project operations' conditions are their record; once written it stays, emptied, until
        // the Project goes, so its version only moves forward.
        let ops = ProjectOpsRecord::from_status(status);
        self.write::<ProjectOpsRecord>(name, None, |current| {
            Ok(match (&ops, current) {
                (Some(ops), _) => Some(ops.clone()),
                (None, Some(_)) => Some(ProjectOpsRecord { refused: None, entries: None }),
                (None, None) => None,
            })
        })?;
        self.judge(
            name,
            Queue {
                entries: status.dispatch_queue.clone(),
                error: status.dispatch_queue_error.clone(),
                attention: status.dispatch_queue_attention.clone(),
            },
        );
        Ok(self.current(name)?)
    }

    async fn delete(&self, name: &str) -> Result<(), String> {
        let _writing = locked(&self.writing);
        // The operations record first: a delete stopped between the two leaves a Project with no
        // conditions, never conditions with no Project.
        self.remove::<ProjectOpsRecord>(name)?;
        self.remove::<ProjectDefinition>(name)?;
        let mut judged = locked(&self.judged);
        if judged.remove(name).is_some() {
            self.judged_change(name);
        }
        Ok(())
    }

    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let logged = self.changes.subscribe().map_err(|error| error.to_string())?;
        let mut judged = self.judged_changes.subscribe();
        judged.mark_unchanged();
        // The port is gone once its sender is; the log alone ticks from then on.
        let judged = futures::stream::unfold(judged, |mut judged| async move {
            judged.changed().await.ok()?;
            Some(((), judged))
        });
        Ok(Box::pin(futures::stream::select(logged, judged)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(home: &str) -> Result<Record<Project>, String> {
        Ok(Record {
            name: "widgets".into(),
            home: home.into(),
            version: 1,
            created_at: chrono::DateTime::UNIX_EPOCH,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            spec: flotilla_resources::ProjectSpec::builder().display_name("Widgets".into()).build(),
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
