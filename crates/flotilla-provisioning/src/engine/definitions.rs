//! The declared definition kinds on the engine, behind `[primitives]` (#3824 layer 2b-ii).
//!
//! [`EngineDefinitions`] is the [`KindRecords`] port of each definition kind over the decided
//! store. This home writes its own definitions through the plain-kind port; every home's are read
//! from a resident map kept current by following the log. No old-store record is written or read.
//!
//! A name several homes hold (each host's manifest declaring the fleet's designation) reads as
//! this home's record, else the lexically first home's, as the import placed them (#3977).
//!
//! A record holds only its own fields. Its writer's stamps are its change's writer and items
//! ([`provenance`](super::provenance)), and a record reads with the stamps rebuilt from its last
//! change. One reason is one change: a root's pass at one revision joins the change its first write
//! opened, and a record declared again for a new reason, unchanged, is written again under it.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use flotilla_engine_store::{
    follow::{Followed, Follower},
    plain::{PendingChange, PlainError, PlainLog},
};
use flotilla_resources::{
    definitions::{origin, stamp, DefinitionOrigin},
    engine_records::{KindRecords, LogPosition, Record, Since, Ticks},
    ConvoyEnsure, CredentialGrant, CredentialSpec, CrewDefaults, FleetDesignation, Forge, ImageLayer, ImageLayerResourceSpec, Resource,
    WorkflowTemplate,
};
use flotilla_schema::{Identity, Kind, KindCode, NewChange, Timestamp};
use flotilla_schema_sqlite::{stream::Service, Read, ResourceId, StoreError};
use futures::stream::BoxStream;

use chrono::{DateTime, Utc};

use super::provenance;
use super::reads::created_at;
use super::resume::followed_since;
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
    /// Whether `spec` is retired: admissible, but stored as nothing.
    fn retired(_spec: &SpecOf<Self>) -> bool {
        false
    }
    /// The kind's records in the resident map.
    fn held(records: &Records) -> &Held<Self::Resource>;
    fn held_mut(records: &mut Records) -> &mut Held<Self::Resource>;
}

/// The spec of the resource kind definition kind `K` is served as.
type SpecOf<K> = <<K as Definition>::Resource as Resource>::Spec;

/// The definition kind a resource kind is stored as.
trait Defined: Resource {
    type Stored: Definition<Resource = Self>;
}

/// A kind whose stored form is built from its spec whole.
macro_rules! whole {
    ($($stored:ty => $resource:ty, $field:ident);* $(;)?) => {$(
        impl Definition for $stored {
            type Resource = $resource;
            fn build(_: Option<String>, spec: SpecOf<Self>) -> Result<Self, String> {
                Ok(<$stored>::new(&spec))
            }
            fn spec(&self) -> SpecOf<Self> {
                <$stored>::spec(self)
            }
            fn held(records: &Records) -> &Held<$resource> {
                &records.$field
            }
            fn held_mut(records: &mut Records) -> &mut Held<$resource> {
                &mut records.$field
            }
        }

        impl Defined for $resource {
            type Stored = $stored;
        }
    )*};
}

whole!(
    ForgeDefinition => Forge, forges;
    FleetDesignationDefinition => FleetDesignation, designations;
    CredentialSpecDefinition => CredentialSpec, credential_specs;
    CredentialGrantDefinition => CredentialGrant, credential_grants;
    CrewDefaultsDefinition => CrewDefaults, crew_defaults;
    ConvoyEnsureDefinition => ConvoyEnsure, convoy_ensures;
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
    fn retired(spec: &ImageLayerResourceSpec) -> bool {
        spec.is_retired()
    }
    fn held(records: &Records) -> &Held<ImageLayer> {
        &records.image_layers
    }
    fn held_mut(records: &mut Records) -> &mut Held<ImageLayer> {
        &mut records.image_layers
    }
}

impl Defined for ImageLayer {
    type Stored = ImageLayerDefinition;
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
    fn held(records: &Records) -> &Held<WorkflowTemplate> {
        &records.workflow_templates
    }
    fn held_mut(records: &mut Records) -> &mut Held<WorkflowTemplate> {
        &mut records.workflow_templates
    }
}

impl Defined for WorkflowTemplate {
    type Stored = WorkflowTemplateDefinition;
}

/// The ManifestRoot's code: a root's baselines hold the paths its definitions read with, so a
/// root's change re-reads its home's definitions.
const ROOT: KindCode = <flotilla_state_machines::Resource<ManifestRoot> as Kind>::CODE;

/// The codes the resident map follows: the definition kinds, and the root.
fn followed() -> Vec<KindCode> {
    vec![
        ForgeDefinition::CODE,
        FleetDesignationDefinition::CODE,
        CredentialSpecDefinition::CODE,
        CredentialGrantDefinition::CODE,
        CrewDefaultsDefinition::CODE,
        ImageLayerDefinition::CODE,
        WorkflowTemplateDefinition::CODE,
        ConvoyEnsureDefinition::CODE,
        ROOT,
    ]
}

/// One kind's records at every home, by name and home.
type Held<R> = BTreeMap<(String, String), Record<R>>;

/// Every home's records, by kind.
#[derive(Default)]
struct Records {
    forges: Held<Forge>,
    designations: Held<FleetDesignation>,
    credential_specs: Held<CredentialSpec>,
    credential_grants: Held<CredentialGrant>,
    crew_defaults: Held<CrewDefaults>,
    image_layers: Held<ImageLayer>,
    workflow_templates: Held<WorkflowTemplate>,
    convoy_ensures: Held<ConvoyEnsure>,
}

struct Resident {
    follower: Follower,
    records: Records,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `value`, created at `created_at`, as the resource, stamped as `origin`'s writer stamps it.
fn object<K: Definition>(
    name: String,
    home: String,
    version: u64,
    created_at: DateTime<Utc>,
    value: &K,
    origin: &DefinitionOrigin,
) -> Record<K::Resource> {
    let (labels, annotations) = stamp(origin);
    Record { name, home, version, created_at, labels, annotations, spec: value.spec(), status: None }
}

/// Record `id` as the resource, with the stamps its last change says.
fn record<K: Definition>(
    read: &mut Read<'_>,
    roots: &mut provenance::Roots,
    id: ResourceId,
    name: String,
    home: String,
    version: u64,
    value: &K,
) -> Result<Record<K::Resource>, StoreError> {
    let change = read.last_change(id)?;
    let origin = provenance::origin_of(read, roots, K::NAME, &name, change.as_ref(), value.project())?;
    let created_at = created_at(read, id)?;
    Ok(object(name, home, version, created_at, value, &origin))
}

/// Reads `identity`, of kind `K`, again into `records`: its current row, or none once deleted.
fn reload_as<K: Definition>(read: &mut Read<'_>, records: &mut Records, identity: &Identity) -> Result<(), StoreError> {
    let key = (identity.name.clone(), identity.home.clone());
    let found = match read.resolve(identity.kind, &identity.name, &identity.home)? {
        Some(id) => match read.load::<K>(id)? {
            Some(loaded) => {
                Some(record(read, &mut provenance::Roots::default(), id, loaded.name, loaded.home, loaded.version, &loaded.value)?)
            }
            None => None,
        },
        None => None,
    };
    match found {
        Some(found) => K::held_mut(records).insert(key, found),
        None => K::held_mut(records).remove(&key),
    };
    Ok(())
}

/// Reads `identity` again into `records`, by its kind.
fn reload(read: &mut Read<'_>, records: &mut Records, identity: &Identity) -> Result<(), StoreError> {
    match identity.kind {
        code if code == ForgeDefinition::CODE => reload_as::<ForgeDefinition>(read, records, identity),
        code if code == FleetDesignationDefinition::CODE => reload_as::<FleetDesignationDefinition>(read, records, identity),
        code if code == CredentialSpecDefinition::CODE => reload_as::<CredentialSpecDefinition>(read, records, identity),
        code if code == CredentialGrantDefinition::CODE => reload_as::<CredentialGrantDefinition>(read, records, identity),
        code if code == CrewDefaultsDefinition::CODE => reload_as::<CrewDefaultsDefinition>(read, records, identity),
        code if code == ImageLayerDefinition::CODE => reload_as::<ImageLayerDefinition>(read, records, identity),
        code if code == WorkflowTemplateDefinition::CODE => reload_as::<WorkflowTemplateDefinition>(read, records, identity),
        code if code == ConvoyEnsureDefinition::CODE => reload_as::<ConvoyEnsureDefinition>(read, records, identity),
        _ => Ok(()),
    }
}

/// Every record of kind `K` homed at `home`, in place of what the map held.
fn scan<K: Definition>(read: &mut Read<'_>, roots: &mut provenance::Roots, records: &mut Records, home: &str) -> Result<(), StoreError> {
    K::held_mut(records).retain(|(_, owner), _| owner != home);
    let mut after = None;
    loop {
        let page = read.scan::<K>(home, after, 64)?;
        for id in page.ids {
            if let Some(loaded) = read.load::<K>(id)? {
                let found = record(read, roots, id, loaded.name.clone(), loaded.home, loaded.version, &loaded.value)?;
                K::held_mut(records).insert((loaded.name, home.to_owned()), found);
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

/// Record `name` as several homes' resolve at `home`: its own, else the lexically first home's
/// (the map's order).
fn resolve<V: Clone>(held: &BTreeMap<(String, String), V>, home: &str, name: &str) -> Option<V> {
    let mut copies = named(held, name);
    let first = copies.next()?;
    let chosen = std::iter::once(first).chain(copies).find(|((_, owner), _)| owner == home).unwrap_or(first);
    Some(chosen.1.clone())
}

/// Every home's record `name`, read from the map by name.
fn named<'a, V>(held: &'a BTreeMap<(String, String), V>, name: &'a str) -> impl Iterator<Item = (&'a (String, String), &'a V)> {
    held.range((name.to_owned(), String::new())..).take_while(move |((held_name, _), _)| held_name == name)
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
            resident: Mutex::new(Resident { follower, records: Records::default() }),
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

    fn write<K: Definition>(&self, name: &str, origin: DefinitionOrigin, spec: &SpecOf<K>) -> Result<Record<K::Resource>, String> {
        let value = K::build(provenance::materialized_project(&origin), spec.clone())?;
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
                    return Ok(object(name.to_owned(), self.home.clone(), version, created_at, &value, &origin));
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

impl flotilla_store::EnginePort for EngineDefinitions {
    fn install(self: Arc<Self>, backend: &flotilla_store::ResourceBackend) {
        backend.install_engine_kind::<Forge>(self.clone());
        backend.install_engine_kind::<FleetDesignation>(self.clone());
        backend.install_engine_kind::<CredentialSpec>(self.clone());
        backend.install_engine_kind::<CredentialGrant>(self.clone());
        backend.install_engine_kind::<CrewDefaults>(self.clone());
        backend.install_engine_kind::<ImageLayer>(self.clone());
        backend.install_engine_kind::<WorkflowTemplate>(self.clone());
        backend.install_engine_kind::<ConvoyEnsure>(self);
    }
}

#[async_trait::async_trait]
impl<R: Defined> KindRecords<R> for EngineDefinitions {
    fn codes(&self) -> Vec<KindCode> {
        vec![R::Stored::CODE, ROOT]
    }

    /// From the shared follower of the log.
    fn ticks(&self) -> Ticks {
        Ticks::Followed
    }

    fn home(&self) -> String {
        self.home.clone()
    }

    async fn get(&self, name: &str) -> Result<Option<Record<R>>, String> {
        let resident = self.refresh()?;
        Ok(resolve(R::Stored::held(&resident.records), &self.home, name))
    }

    async fn list(&self) -> Result<Vec<Record<R>>, String> {
        let resident = self.refresh()?;
        let held = R::Stored::held(&resident.records);
        let mut names: Vec<&String> = held.keys().map(|(name, _)| name).collect();
        // The map orders by name, then home, so one name's homes are adjacent.
        names.dedup();
        Ok(names.into_iter().filter_map(|name| resolve(held, &self.home, name)).collect())
    }

    async fn sources(&self) -> Result<Vec<Record<R>>, String> {
        let resident = self.refresh()?;
        Ok(R::Stored::held(&resident.records).values().cloned().collect())
    }

    async fn positioned(&self) -> Result<(Vec<Record<R>>, LogPosition), String> {
        let resident = self.refresh()?;
        Ok((R::Stored::held(&resident.records).values().cloned().collect(), LogPosition::logged(resident.follower.cursors().clone())))
    }

    async fn position(&self) -> Result<LogPosition, String> {
        Ok(LogPosition::logged(self.refresh()?.follower.cursors().clone()))
    }

    /// A root's change names every record of its home: its baselines hold the paths they read with.
    async fn changed_since(&self, position: &LogPosition) -> Result<Since, String> {
        let (changed, through) = match followed_since(&self.service, [R::Stored::CODE, ROOT], position)? {
            Ok(followed) => followed,
            Err(reason) => return Ok(Since::Expired(reason)),
        };
        let resident = self.refresh()?;
        let held = R::Stored::held(&resident.records);
        let mut names = BTreeSet::new();
        for identity in changed {
            if identity.kind == ROOT {
                names.extend(held.keys().filter(|(_, home)| *home == identity.home).map(|(name, _)| name.clone()));
            } else {
                names.insert(identity.name);
            }
        }
        Ok(Since::Changed { names, through: LogPosition::logged(through) })
    }

    /// Every home's record `name`, read from the resident map by name.
    async fn copies(&self, name: &str) -> Result<Vec<Record<R>>, String> {
        let resident = self.refresh()?;
        Ok(named(R::Stored::held(&resident.records), name).map(|(_, held)| held.clone()).collect())
    }

    async fn apply(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
        annotations: &BTreeMap<String, String>,
        spec: &R::Spec,
    ) -> Result<Record<R>, String> {
        let origin = provenance::resolve_stamped_root(&self.service, &self.home, origin(labels, annotations)?)?;
        if R::Stored::retired(spec) {
            // ADR 0053's retired layers stay admissible for one roll, so that previous-generation
            // manifests still apply, and are inert for resolution. The import drops them; so does
            // a write: this home's copy is removed, and the written value is returned unstored.
            self.remove::<R::Stored>(name)?;
            let (labels, annotations) = stamp(&origin);
            return Ok(Record {
                name: name.to_owned(),
                home: self.home.clone(),
                version: 0,
                // Not stored, so created by this write.
                created_at: self.clock.now(),
                labels,
                annotations,
                spec: spec.clone(),
                status: None,
            });
        }
        self.write::<R::Stored>(name, origin, spec)
    }

    async fn set_status(&self, name: &str, _status: &R::Status) -> Result<Record<R>, String> {
        Err(format!("{} {name}: a definition has no status", R::API_PATHS.kind))
    }

    async fn delete(&self, name: &str) -> Result<(), String> {
        self.remove::<R::Stored>(name)
    }

    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        self.changes.subscribe().map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Behaviour (#3824 layer 2b-ii, as the import placed records): a name several homes hold reads
    // as this home's record, else the lexically first home's; another name's never.
    #[test]
    fn a_name_several_homes_hold_reads_as_this_homes_else_the_first() {
        let records: BTreeMap<(String, String), String> =
            ["udder", "feta", "kiwi"].into_iter().map(|home| (("fleet".to_owned(), home.to_owned()), home.to_owned())).collect();
        let home_of = |at: &str| resolve(&records, at, "fleet").expect("held");
        assert_eq!(home_of("kiwi"), "kiwi");
        assert_eq!(home_of("b5"), "feta");
        assert!(resolve(&records, "kiwi", "other").is_none());
    }
}
