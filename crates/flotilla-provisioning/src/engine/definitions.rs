//! The declared definition kinds on the engine, behind `[primitives]` (#3824 layer 2b-ii).
//!
//! [`EngineDefinitions`] is the [`EngineRecords`] port of the definition kinds over the decided store. This home
//! writes its own definitions through the plain-kind port; every home's are read from a resident
//! map kept current by following the log. No old-store record is written or read.
//!
//! A name several homes hold (each host's manifest declaring the fleet's designation) reads as
//! this home's record, else the lexically first home's, as the import placed them (#3977).
//!
//! A record holds only its own fields. Its writer's stamps are its change's writer and items
//! ([`provenance`](super::provenance)), and a record reads with the stamps rebuilt from its last
//! change. One reason is one change: a root's pass at one revision joins the change its first write
//! opened, and a record declared again for a new reason, unchanged, is written again under it.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use flotilla_engine_store::{
    follow::{Followed, Follower},
    plain::{PendingChange, PlainError, PlainLog},
};
use flotilla_resources::{
    definitions::{origin, stamp, DefinitionOrigin, DEFINITION_KINDS},
    engine_records::{decode_spec, EngineObject, EngineRecords, Record},
    ConvoyEnsure, CredentialGrant, CredentialSpec, CrewDefaults, FleetDesignation, Forge, ImageLayer, ImageLayerResourceSpec, Resource,
    WorkflowTemplate,
};
use flotilla_schema::{Identity, Kind, KindCode, NewChange, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, ResourceId, StoreError};
use futures::stream::BoxStream;
use serde_json::Value;

use chrono::{DateTime, Utc};

use super::provenance;
use super::reads::created_at;
use super::watch::CommitsSeen;
use super::watch::SharedChanges;
use crate::stored::definition::{
    ConvoyEnsureDefinition, CredentialGrantDefinition, CredentialSpecDefinition, CrewDefaultsDefinition, FleetDesignationDefinition,
    ForgeDefinition, ImageLayerDefinition, WorkflowTemplateDefinition,
};
use crate::stored::manifest_root::ManifestRoot;

/// How often a write that loses a race to another writer of its name reads and tries again.
const WRITE_ATTEMPTS: usize = 3;

/// The writer of a delete, which names no origin: the port deletes what its callers remove.
const WRITER: &str = "definition-records";

/// A definition kind as the port serves it: its resource kind's spec in and out.
trait Definition: Kind + PartialEq + Sized {
    /// The resource kind it is served as.
    type Resource: Resource;
    /// The resource kind's name.
    const NAME: &'static str = <Self::Resource as Resource>::API_PATHS.kind;
    fn build(project: Option<String>, spec: SpecOf<Self>) -> Result<Self, String>;
    /// The project a record was materialised for: a workflow template's `project` column.
    fn project(&self) -> Option<&str> {
        None
    }
    fn spec(&self) -> SpecOf<Self>;
}

/// The spec of the resource kind definition kind `K` is served as.
type SpecOf<K> = <<K as Definition>::Resource as Resource>::Spec;

/// A kind whose stored form is built from its spec whole.
macro_rules! whole {
    ($($stored:ty => $resource:ty);* $(;)?) => {$(
        impl Definition for $stored {
            type Resource = $resource;
            fn build(_: Option<String>, spec: SpecOf<Self>) -> Result<Self, String> {
                Ok(<$stored>::new(&spec))
            }
            fn spec(&self) -> SpecOf<Self> {
                <$stored>::spec(self)
            }
        }
    )*};
}

whole!(
    ForgeDefinition => Forge;
    FleetDesignationDefinition => FleetDesignation;
    CredentialSpecDefinition => CredentialSpec;
    CredentialGrantDefinition => CredentialGrant;
    CrewDefaultsDefinition => CrewDefaults;
    ConvoyEnsureDefinition => ConvoyEnsure;
);

impl Definition for ImageLayerDefinition {
    type Resource = ImageLayer;
    fn build(_: Option<String>, spec: ImageLayerResourceSpec) -> Result<Self, String> {
        match spec {
            ImageLayerResourceSpec::Current(layer) => ImageLayerDefinition::new(&layer),
            // Handled before building: see `EngineDefinitions::apply`.
            ImageLayerResourceSpec::Retired => Err("a retired ImageLayer has no stored form".into()),
        }
    }
    fn spec(&self) -> ImageLayerResourceSpec {
        ImageLayerResourceSpec::Current(Box::new(ImageLayerDefinition::spec(self).clone()))
    }
}

impl Definition for WorkflowTemplateDefinition {
    type Resource = WorkflowTemplate;
    fn build(project: Option<String>, spec: SpecOf<Self>) -> Result<Self, String> {
        WorkflowTemplateDefinition::new(project, &spec)
    }
    fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }
    fn spec(&self) -> SpecOf<Self> {
        WorkflowTemplateDefinition::spec(self).clone()
    }
}

/// Runs `$body` with `$k` the stored type of resource kind `$kind`, or refuses another kind.
macro_rules! by_kind {
    ($kind:expr, |$k:ident| $body:expr) => {
        match $kind {
            "Forge" => {
                type $k = ForgeDefinition;
                $body
            }
            "FleetDesignation" => {
                type $k = FleetDesignationDefinition;
                $body
            }
            "CredentialSpec" => {
                type $k = CredentialSpecDefinition;
                $body
            }
            "CredentialGrant" => {
                type $k = CredentialGrantDefinition;
                $body
            }
            "CrewDefaults" => {
                type $k = CrewDefaultsDefinition;
                $body
            }
            "ImageLayer" => {
                type $k = ImageLayerDefinition;
                $body
            }
            "WorkflowTemplate" => {
                type $k = WorkflowTemplateDefinition;
                $body
            }
            "ConvoyEnsure" => {
                type $k = ConvoyEnsureDefinition;
                $body
            }
            other => Err(format!("{other} is not a definition kind")),
        }
    };
}

/// The definition kinds' codes, with their resource kind names.
fn codes() -> [(KindCode, &'static str); 8] {
    [
        (ForgeDefinition::CODE, ForgeDefinition::NAME),
        (FleetDesignationDefinition::CODE, FleetDesignationDefinition::NAME),
        (CredentialSpecDefinition::CODE, CredentialSpecDefinition::NAME),
        (CredentialGrantDefinition::CODE, CredentialGrantDefinition::NAME),
        (CrewDefaultsDefinition::CODE, CrewDefaultsDefinition::NAME),
        (ImageLayerDefinition::CODE, ImageLayerDefinition::NAME),
        (WorkflowTemplateDefinition::CODE, WorkflowTemplateDefinition::NAME),
        (ConvoyEnsureDefinition::CODE, ConvoyEnsureDefinition::NAME),
    ]
}

/// The ManifestRoot's code: a root's baselines hold the paths its definitions read with, so a
/// root's change re-reads its home's definitions.
const ROOT: KindCode = <flotilla_state_machines::Resource<ManifestRoot> as Kind>::CODE;

/// The codes the resident map follows: the definition kinds, and the root.
fn followed() -> Vec<KindCode> {
    codes().map(|(code, _)| code).into_iter().chain([ROOT]).collect()
}

fn kind_name(code: KindCode) -> Option<&'static str> {
    codes().into_iter().find(|(held, _)| *held == code).map(|(_, name)| name)
}

/// One home's record as the resident map holds it: the old object it reads as, or why it does not
/// encode.
type Held = Result<EngineObject, String>;

/// Every home's records, by kind, name and home.
type Records = BTreeMap<(&'static str, String, String), Held>;

struct Resident {
    follower: Follower,
    records: Records,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `value`, created at `created_at`, as the old object, stamped as `origin`'s writer stamps it.
fn object<K: Definition>(
    name: String,
    home: String,
    version: u64,
    created_at: DateTime<Utc>,
    value: &K,
    origin: &DefinitionOrigin,
) -> Held {
    let (labels, annotations) = stamp(origin);
    Record::<K::Resource> { name, home, version, created_at, labels, annotations, spec: value.spec(), status: None }.engine_object()
}

/// Record `id` as the old object, with the stamps its last change says.
fn record<K: Definition>(
    read: &mut Read<'_>,
    roots: &mut provenance::Roots,
    id: ResourceId,
    name: String,
    home: String,
    version: u64,
    value: &K,
) -> Result<Held, StoreError> {
    let change = read.last_change(id)?;
    let origin = provenance::origin_of(read, roots, K::NAME, &name, change.as_ref(), value.project())?;
    let created_at = created_at(read, id)?;
    Ok(object(name, home, version, created_at, value, &origin))
}

/// The current row of `id` of kind `K`, if any.
fn load<K: Definition>(read: &mut Read<'_>, id: Option<ResourceId>) -> Result<Option<Held>, StoreError> {
    let Some(id) = id else { return Ok(None) };
    let Some(loaded) = read.load::<K>(id)? else { return Ok(None) };
    record(read, &mut provenance::Roots::default(), id, loaded.name, loaded.home, loaded.version, &loaded.value).map(Some)
}

/// Reads `identity` again into `records`: its current row, or none once deleted.
fn reload(read: &mut Read<'_>, records: &mut Records, identity: &Identity) -> Result<(), StoreError> {
    let Some(kind) = kind_name(identity.kind) else { return Ok(()) };
    let key = (kind, identity.name.clone(), identity.home.clone());
    let id = read.resolve(identity.kind, &identity.name, &identity.home)?;
    let found = match kind {
        "Forge" => load::<ForgeDefinition>(read, id)?,
        "FleetDesignation" => load::<FleetDesignationDefinition>(read, id)?,
        "CredentialSpec" => load::<CredentialSpecDefinition>(read, id)?,
        "CredentialGrant" => load::<CredentialGrantDefinition>(read, id)?,
        "CrewDefaults" => load::<CrewDefaultsDefinition>(read, id)?,
        "ImageLayer" => load::<ImageLayerDefinition>(read, id)?,
        "ConvoyEnsure" => load::<ConvoyEnsureDefinition>(read, id)?,
        _ => load::<WorkflowTemplateDefinition>(read, id)?,
    };
    match found {
        Some(found) => records.insert(key, found),
        None => records.remove(&key),
    };
    Ok(())
}

/// Every record of kind `K` homed at `home`.
fn scan<K: Definition>(read: &mut Read<'_>, roots: &mut provenance::Roots, records: &mut Records, home: &str) -> Result<(), StoreError> {
    let mut after = None;
    loop {
        let page = read.scan::<K>(home, after, 64)?;
        for id in page.ids {
            if let Some(loaded) = read.load::<K>(id)? {
                let found = record(read, roots, id, loaded.name.clone(), loaded.home, loaded.version, &loaded.value)?;
                records.insert((K::NAME, loaded.name, home.to_owned()), found);
            }
        }
        after = page.next;
        if after.is_none() {
            return Ok(());
        }
    }
}

/// Reads every record homed at `home` into `records`, in place of what they held.
fn rescan(read: &mut Read<'_>, records: &mut Records, home: &str) -> Result<(), StoreError> {
    records.retain(|(_, _, owner), _| owner != home);
    // One read of each root the home's definitions name, whatever their number.
    let roots = &mut provenance::Roots::default();
    scan::<ForgeDefinition>(read, roots, records, home)?;
    scan::<FleetDesignationDefinition>(read, roots, records, home)?;
    scan::<CredentialSpecDefinition>(read, roots, records, home)?;
    scan::<CredentialGrantDefinition>(read, roots, records, home)?;
    scan::<CrewDefaultsDefinition>(read, roots, records, home)?;
    scan::<ImageLayerDefinition>(read, roots, records, home)?;
    scan::<WorkflowTemplateDefinition>(read, roots, records, home)?;
    scan::<ConvoyEnsureDefinition>(read, roots, records, home)
}

/// `kind`'s record `name` as several homes' resolve at `home`: its own, else the lexically first
/// home's (the map's order).
fn resolve(records: &Records, home: &str, kind: &str, name: &str) -> Option<Held> {
    let mut held = named(records, kind, name);
    let first = held.next()?;
    let chosen = std::iter::once(first).chain(held).find(|((_, _, owner), _)| owner == home).unwrap_or(first);
    Some(chosen.1.clone())
}

/// Every home's record `name` of `kind`, read from the map by name.
fn named<'a>(records: &'a Records, kind: &str, name: &'a str) -> impl Iterator<Item = (&'a (&'static str, String, String), &'a Held)> {
    let start = DEFINITION_KINDS.iter().find(|held| **held == kind).map(|kind| (*kind, name.to_owned(), String::new()));
    start.into_iter().flat_map(move |start| {
        let kind = start.0;
        records.range(start..).take_while(move |((held, held_name, _), _)| *held == kind && held_name == name)
    })
}

/// The definition records of the decided store at one home.
pub struct EngineDefinitions {
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

impl std::fmt::Debug for EngineDefinitions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("EngineDefinitions").field("home", &self.home).finish_non_exhaustive()
    }
}

impl EngineDefinitions {
    /// Opens this home's definition writer over `service`, and reads every home's records.
    pub fn open(service: Arc<Service>, clock: Arc<dyn flotilla_resources::Clock>) -> Result<Self, String> {
        let log = PlainLog::new(service.store());
        let home = log.home().to_owned();
        let follower = Follower::new(followed());
        let opened = Self {
            changes: SharedChanges::new(Arc::clone(&service), followed(), "definitions"),
            commits: CommitsSeen::follow(&service, followed()),
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
                            Followed::Changed(identity) => reload(&mut read, records, &identity)?,
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

    /// `kind`'s record `name` as several homes' resolve: this home's, else the first home's.
    fn resolve(&self, records: &Records, kind: &str, name: &str) -> Option<Held> {
        resolve(records, &self.home, kind, name)
    }

    fn write<K: Definition>(&self, name: &str, origin: DefinitionOrigin, spec: Value) -> Result<EngineObject, String> {
        let value = K::build(provenance::materialized_project(&origin), decode_spec::<K::Resource>(name, spec)?)?;
        // Writers of one name at this home are not serialised (the manifest reconciler, the CLI,
        // project ops): a write that loses a race reads again and writes over the winner, as the
        // old store's last writer won.
        for _ in 0..WRITE_ATTEMPTS {
            let change = PendingChange::joining(provenance::change_of(&origin, &self.home, self.now()));
            let written = match self.log.get::<K>(name, &self.home).map_err(|error| error.to_string())? {
                // An unchanged record declared again for the same reason logs nothing; for a new
                // reason, such as a manifest's next revision, it is written again under that change.
                Some(current) if current.spec == value => {
                    // Read apart from the record: another write between the two moves its version,
                    // so the touch below is refused as a conflict and the loop reads again.
                    let last = self.log.last_change::<K>(name, &self.home).map_err(|error| error.to_string())?;
                    if last.is_some_and(|last| last.writer == change.change().writer && last.items == change.change().items) {
                        Ok(current.version)
                    } else {
                        self.log.touch::<K>(name, current.version, &change)
                    }
                }
                Some(current) => self.log.edit(name, current.version, &value, &change),
                None => self.log.create(name, &value, &change),
            };
            match written {
                Ok(version) => {
                    let created_at = self.log.created::<K>(name, &self.home).map_err(|error| error.to_string())?.to_datetime();
                    return object(name.to_owned(), self.home.clone(), version, created_at, &value, &origin);
                }
                Err(PlainError::Conflict { .. } | PlainError::Exists { .. } | PlainError::Missing { .. }) => continue,
                Err(error) => return Err(format!("write {} {name}: {error}", K::NAME)),
            }
        }
        Err(format!("write {} {name}: lost {WRITE_ATTEMPTS} races with concurrent writers", K::NAME))
    }

    fn remove<K: Definition>(&self, name: &str) -> Result<(), String> {
        let Some(current) = self.log.get::<K>(name, &self.home).map_err(|error| error.to_string())? else { return Ok(()) };
        match self.log.delete::<K>(name, current.version, &PendingChange::new(NewChange::new(WRITER, self.now()))) {
            Ok(()) | Err(PlainError::Missing { .. }) => Ok(()),
            Err(error) => Err(format!("delete {} {name}: {error}", K::NAME)),
        }
    }
}

#[async_trait::async_trait]
impl EngineRecords for EngineDefinitions {
    fn kinds(&self) -> &'static [&'static str] {
        &DEFINITION_KINDS
    }

    fn codes(&self, kind: &str) -> Vec<KindCode> {
        codes().into_iter().filter(|(_, name)| *name == kind).map(|(code, _)| code).chain([ROOT]).collect()
    }

    fn home(&self) -> String {
        self.home.clone()
    }

    async fn get(&self, kind: &str, name: &str) -> Result<Option<EngineObject>, String> {
        let resident = self.refresh()?;
        self.resolve(&resident.records, kind, name).transpose()
    }

    async fn list(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        let resident = self.refresh()?;
        let mut names: Vec<&String> = resident.records.keys().filter(|(held, _, _)| *held == kind).map(|(_, name, _)| name).collect();
        // The map orders by kind, then name, then home, so one name's homes are adjacent.
        names.dedup();
        // A record that does not encode is skipped with a warning, so it cannot hide the kind's
        // others; a read of its own name reports it.
        Ok(names
            .into_iter()
            .filter_map(|name| self.resolve(&resident.records, kind, name))
            .filter_map(|held| held.map_err(|error| tracing::warn!(%error, "skipping a definition record that does not encode")).ok())
            .collect())
    }

    async fn sources(&self, kind: &str) -> Result<Vec<EngineObject>, String> {
        let resident = self.refresh()?;
        Ok(resident
            .records
            .iter()
            .filter(|((held, _, _), _)| *held == kind)
            .filter_map(|(_, held)| {
                held.clone().map_err(|error| tracing::warn!(%error, "skipping a definition record that does not encode")).ok()
            })
            .collect())
    }

    /// Every home's record `name`, read from the resident map by name.
    async fn copies(&self, kind: &str, name: &str) -> Result<Vec<EngineObject>, String> {
        let resident = self.refresh()?;
        Ok(named(&resident.records, kind, name)
            .filter_map(|(_, held)| {
                held.clone().map_err(|error| tracing::warn!(%error, "skipping a definition record that does not encode")).ok()
            })
            .collect())
    }

    async fn apply(
        &self,
        kind: &str,
        name: &str,
        labels: &BTreeMap<String, String>,
        annotations: &BTreeMap<String, String>,
        spec: Value,
    ) -> Result<EngineObject, String> {
        let origin = provenance::resolve_stamped_root(&self.service, &self.home, origin(labels, annotations)?)?;
        if kind == ImageLayerDefinition::NAME {
            let layer = decode_spec::<ImageLayer>(name, spec.clone())?;
            if layer.is_retired() {
                // ADR 0053's retired layers stay admissible for one roll, so that previous-generation
                // manifests still apply, and are inert for resolution. The import drops them; so
                // does a write: this home's copy is removed, and the written value is returned
                // unstored.
                self.remove::<ImageLayerDefinition>(name)?;
                let (labels, annotations) = stamp(&origin);
                return Record::<ImageLayer> {
                    name: name.to_owned(),
                    home: self.home.clone(),
                    version: 0,
                    // Not stored, so created by this write.
                    created_at: self.clock.now(),
                    labels,
                    annotations,
                    spec: layer,
                    status: None,
                }
                .engine_object();
            }
        }
        by_kind!(kind, |K| self.write::<K>(name, origin, spec))
    }

    async fn set_status(&self, kind: &str, name: &str, _status: Value) -> Result<EngineObject, String> {
        Err(format!("{kind} {name}: a definition has no status"))
    }

    async fn delete(&self, kind: &str, name: &str) -> Result<(), String> {
        by_kind!(kind, |K| self.remove::<K>(name))
    }

    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        self.changes.subscribe().map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(home: &str) -> Held {
        Ok(EngineObject {
            name: "fleet".into(),
            home: home.into(),
            version: 1,
            created_at: chrono::DateTime::UNIX_EPOCH,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            spec: Value::Null,
            status: None,
        })
    }

    // Behaviour (#3824 layer 2b-ii, as the import placed records): a name several homes hold reads
    // as this home's record, else the lexically first home's; another kind's or name's never.
    #[test]
    fn a_name_several_homes_hold_reads_as_this_homes_else_the_first() {
        let records: Records = ["udder", "feta", "kiwi"]
            .into_iter()
            .map(|home| (("FleetDesignation", "fleet".to_owned(), home.to_owned()), held(home)))
            .chain([(("Forge", "fleet".to_owned(), "aaa".to_owned()), held("aaa"))])
            .collect();
        let home_of = |at: &str| resolve(&records, at, "FleetDesignation", "fleet").expect("held").expect("encodes").home;
        assert_eq!(home_of("kiwi"), "kiwi");
        assert_eq!(home_of("b5"), "feta");
        assert!(resolve(&records, "kiwi", "FleetDesignation", "other").is_none());
    }
}
